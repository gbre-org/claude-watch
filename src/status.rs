//! Claude Code status parsing and watcher/process checks.

use crate::cmd::{run_cmd, run_cmd_any};
use regex_lite::Regex;
use serde::Serialize;
use std::time::SystemTime;
use tracing::{debug, warn};

/// Parsed Claude Code status from tmux pane capture + /proc.
#[derive(Debug, Serialize, Clone)]
pub struct ClaudeStatus {
    pub pane: String,
    pub tokens: u64,
    pub bashes: u64,
    /// True when the pane showed active-work UI markers (thinking indicator,
    /// agent-roster rows, or the Background-tasks overlay) at capture time.
    /// Positive proof the session is alive even when the bare context total
    /// could not be parsed (`tokens == 0` is then a parse MISS, not a fresh
    /// session). See `pane_shows_active_ui`. (operator #5620)
    pub active_ui: bool,
    pub compact_remaining: Option<u32>,
    pub version: Option<String>,
    pub latest: Option<String>,
}

/// Parsed status bar fields (pure data, no I/O).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ParsedStatusBar {
    pub tokens: Option<u64>,
    pub bashes: Option<u64>,
    pub compact_remaining: Option<u32>,
}

/// Version info from /proc and symlinks.
#[derive(Debug, Default)]
pub struct VersionInfo {
    pub running: Option<String>,
    pub installed: Option<String>,
}

/// How a watcher is launched and how its output reaches the main loop.
///
/// * `Oneshot` (the default, the historical contract): `watcher-ctl run
///   <name>` is spawned as a background Bash task; the watcher blocks, prints
///   one batch, EXITS, and the task-completion notification delivers the
///   captured stdout. Every batch costs a restart.
/// * `Monitor`: the watcher is armed ONCE, from the main loop, through a
///   line-streaming launcher (Claude Code's `Monitor` tool) and stays alive
///   across batches; each stdout line is its own notification. `watcher-ctl
///   run <name>` then does NOT exec the one-shot — it prints the exact command
///   to arm and records the intent — while `status`/`list` keep treating a
///   live pid as healthy via the same pidfile model as any other watcher.
///
/// Flipping between them is ONE config edit + re-arm; nothing is rebuilt or
/// reverted and no session restart is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WatcherMode {
    #[default]
    Oneshot,
    Monitor,
}

impl WatcherMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            WatcherMode::Oneshot => "oneshot",
            WatcherMode::Monitor => "monitor",
        }
    }

    /// Parse a config value. Accepts the canonical `oneshot` / `monitor` plus
    /// the spellings a human is likely to type (`one-shot`, `exit` — the
    /// watcher script's own name for the block-print-exit shape). `None` for
    /// anything else, which callers treat as "unset" (default Oneshot) so a
    /// typo degrades to today's behaviour rather than to a blackholed watcher.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "oneshot" | "one-shot" | "exit" | "block-print-exit" => Some(WatcherMode::Oneshot),
            "monitor" => Some(WatcherMode::Monitor),
            _ => None,
        }
    }
}

/// Watcher config entry parsed from watchers.conf (base layer + optional
/// override layer; see [`load_watchers_config`]).
#[derive(Debug, Clone, Default)]
pub struct WatcherEntry {
    pub name: String,
    pub pattern: String,
    pub min_count: u32,
    pub enabled: bool,
    pub start_cmd: Option<String>,
    /// Optional restart-handler command (shell-style). If set, runs
    /// immediately before the watcher's start_cmd whenever a stale PID
    /// file is present (i.e. the watcher previously exited and is being
    /// brought back up). Lets operators wire site-specific
    /// "show me what I missed" behavior (e.g. dump the last N minutes
    /// of message history) without baking integration names into the
    /// daemon.
    pub on_restart_cmd: Option<String>,
    /// Delivery mode — see [`WatcherMode`]. Field 7 (`mode`) of a conf line.
    pub mode: WatcherMode,
    /// Command the main loop arms under the line-streaming launcher when
    /// `mode=monitor`. Field 8 (`monitor_cmd`). When unset, the effective
    /// command is `<start_cmd> --mode monitor` — the convention the reference
    /// watcher (`claude-event-watch`) implements; a watcher with a different
    /// monitor flag sets this explicitly.
    pub monitor_cmd: Option<String>,
    /// Which config layer INTRODUCED this entry: `"base"` or `"override"`.
    /// Purely informational (shown by `watcher-ctl list`).
    pub layer: String,
    /// Field names the override layer CHANGED on a base entry (e.g.
    /// `["mode", "enabled"]`). Empty when the entry is exactly what the base
    /// file says. Shown by `watcher-ctl list` so "which layer won" is visible.
    pub overridden: Vec<String>,
}

impl WatcherEntry {
    /// The command to arm under a line-streaming launcher when this watcher
    /// is in monitor mode: the explicit `monitor_cmd` if set, else
    /// `<start_cmd> --mode monitor`. `None` when neither is derivable.
    pub fn effective_monitor_cmd(&self) -> Option<String> {
        if let Some(m) = self.monitor_cmd.as_deref() {
            let m = m.trim();
            if !m.is_empty() {
                return Some(m.to_string());
            }
        }
        self.start_cmd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("{} --mode monitor", s))
    }
}

/// Layer label for entries that come from the primary watchers.conf.
pub const WATCHER_LAYER_BASE: &str = "base";
/// Layer label for entries introduced (not merely modified) by the override file.
pub const WATCHER_LAYER_OVERRIDE: &str = "override";

/// Pure function: parse status bar fields from pane capture text.
///
/// Looks at the last 10 lines for:
/// - Token count: `(\d[\d,]*)\s+tokens` (also handles k/M suffix in thinking
///   indicator: `↑ 2.3k tokens`, `↓ 1.4M tokens`).
/// - Bash/background task count: `(\d+)\s+(?:bashes|background\s+tasks|shells?)`
///   — singular `shell`/`bash`/`task` accepted (status bar emits "1 shell",
///   not "1 shells"); also tolerates an "active" qualifier from the overlay
///   panel ("4 active shells").
/// - Compact remaining: `Context left until auto-compact:\s*(\d+)%`
///
/// Returns `(parsed, saw_status_bar)`. `saw_status_bar` indicates we found
/// a status-bar indicator anywhere in the tail — used by the parse-miss
/// detector to distinguish "real status bar with no counts" (legitimate
/// idle state, e.g. ⏵⏵ bar with 0 shells and 0 tokens) from "no status
/// bar visible at all" (overlay panel etc.).
pub(crate) fn parse_status_bar(pane_text: &str) -> ParsedStatusBar {
    parse_status_bar_with_diag(pane_text).0
}

/// Is this line one of the agent-roster rows Claude Code draws below the
/// status bar, one per running subagent?
///
/// Shape (real capture, 2026-08-20):
/// ```text
///   ● main
///   ◯ general-purpose    Scanning claude-w… 3m 14s · ↓ 102.8k tokens
/// ```
/// The trailing count belongs to THAT AGENT, not to the session, so the
/// token parser must never mistake it for the session context size.
///
/// The thinking indicator uses the same bullet glyph in some Claude Code
/// versions (`● Zigzagging… (37s · ↓ 1.3k tokens · thought for 13s)`) but
/// always parenthesises its counters, while a roster row never does — so
/// the `(` test separates them without depending on which glyph is in
/// fashion.
pub(crate) fn is_agent_roster_row(line: &str) -> bool {
    let trimmed = line.trim_start();
    let starts_with_bullet = trimmed.starts_with('\u{25ef}') // ◯ — idle/running agent
        || trimmed.starts_with('\u{25cf}'); // ● — selected / main row
    starts_with_bullet
        && !trimmed.contains('(')
        && (trimmed.contains('\u{2191}') || trimmed.contains('\u{2193}'))
        && trimmed.contains("tok")
}

/// Pure predicate: does the pane show ACTIVE-WORK UI markers — a thinking
/// indicator (`↑/↓ N tokens`), one or more agent-roster rows, or the
/// "Background tasks" overlay?
///
/// These markers are drawn ONLY while the session is actively generating or
/// has live subagents / background tasks; they NEVER appear on a genuinely
/// fresh, idle session sitting at an empty `❯` prompt. So when the status
/// parser cannot read the bare context total (it has scrolled out of the
/// capture window behind these very markers) and returns `tokens == 0`, this
/// predicate is the positive-liveness signal that separates a live session
/// with an off-screen total (a parse MISS) from a genuinely fresh/empty one.
/// The dead-process / fresh-external-session inject path consults it so a
/// long, active session is never misread as fresh and spuriously handed the
/// resume-checklist prompt (recurring false-positive, operator #5620).
///
/// Scans the WHOLE pane (markers can sit well above the bottom-10 window when
/// an overlay panel pushes the tail up), mirroring the whole-pane marker scan
/// in `parse_status_bar_with_diag`.
///
/// Also recognizes the completion-tail line Claude Code prints after a
/// thinking burst ends while background work (shells, background tasks, or
/// Monitor-tool watches) is still outstanding — e.g. `✻ Brewed for 47m 32s ·
/// 2 monitors still running` — and the bare `· N monitors ·` status-bar
/// counter. Both are positive proof of a live, non-fresh session even when
/// the bare context total is off-screen (2026-08-27 regression: Andrew's
/// screenshot showed the "fresh session" resume prompt fire mid-session on
/// exactly this line, because neither the completion-tail phrasing nor the
/// "monitors" counter word were recognized as active-work markers).
pub(crate) fn pane_shows_active_ui(pane_text: &str) -> bool {
    let thinking_re =
        Regex::new(r"[\u{2191}\u{2193}]\s*\d[\d,.]*\s*[kKmM]?\s*tok").unwrap();
    // Bare status-bar concurrent-task counters that are NOT already covered
    // by a more specific check below (`monitor(?:s)?` mirrors the
    // `bash_re` alternation added to `parse_status_bar_with_diag`'s
    // token/task parser for the same underlying regression).
    let counter_re = Regex::new(r"\d+\s+(?:active\s+)?monitors?\b").unwrap();
    pane_text.lines().any(|line| {
        is_agent_roster_row(line)
            || thinking_re.is_match(line)
            || counter_re.is_match(line)
            || line.contains("Background tasks")
            || line.contains("active shells")
            || line.contains("active shell")
            || line.contains("active agents")
            || line.contains("active agent")
            || line.contains("Local agents")
            || line.contains(" Shells (")
            // Completion-tail "N <shells|background tasks|monitors> still
            // running" (or the pane-width-truncated "still…" form) — printed
            // whenever background work outlives the thinking burst that
            // preceded it, regardless of which noun names the work.
            || line.contains("still running")
            || line.contains("still\u{2026}")
    })
}

/// Like `parse_status_bar` but also returns whether a status-bar marker was
/// detected in the tail. The marker flag lets `is_parse_miss` suppress
/// warnings for legitimately-idle status bars (which carry neither tokens
/// nor a shell count).
pub(crate) fn parse_status_bar_with_diag(pane_text: &str) -> (ParsedStatusBar, bool) {
    let mut result = ParsedStatusBar::default();
    // Seen-but-not-trusted markers: a thinking-indicator or agent-roster
    // line proves *some* Claude Code status UI was on screen (so
    // `is_parse_miss` shouldn't complain), but per the fix below neither
    // one's number is ever allowed into `result.tokens`.
    let mut saw_thinking_indicator = false;
    let mut saw_agent_roster = false;

    let lines: Vec<&str> = pane_text.lines().collect();
    let start = if lines.len() > 10 {
        lines.len() - 10
    } else {
        0
    };

    // Match "N tokens" or truncated "N tok…" / "N toke" — but ONLY on status
    // bar lines (contain permission mode, INSERT, or background tasks
    // indicator). This prevents matching thinking indicator text ("↓ 400
    // tokens") or Claude's output text that mentions tokens.
    //
    // Claude Code truncates the status bar with an ellipsis when the pane is
    // narrow, producing `502064 tok…` (only three letters of "tokens"). We
    // match `tok` followed by anything that is NOT a letter — that excludes
    // false positives like "took" / "token" in prose while still catching
    // both the truncated and full forms.
    let token_re = Regex::new(r"(\d[\d,]*)\s+tok").unwrap();
    // Thinking-indicator format: `↑ 2.3k tokens`, `↓ 1.4M tokens`, `↑ 286 tokens`.
    // The arrow + suffix combination is unique to Claude Code's
    // thinking/streaming line and never appears in chat prose. Match it
    // explicitly so we still get a token count when the status bar itself
    // has been clobbered by an overlay panel or extreme wrap.
    let token_thinking_re =
        Regex::new(r"[\u{2191}\u{2193}]\s*([\d]+(?:\.[\d]+)?)\s*([kKmM]?)\s*tok").unwrap();
    // ARROW-PREFIXED counters are NOT the session context total.
    //
    // Two different UI elements render `<arrow> N tokens`:
    //   * the thinking indicator — output tokens streamed in the CURRENT turn;
    //   * the agent-roster rows Claude Code draws BELOW the status bar, one
    //     per running subagent (`◯ general-purpose  Scanning… 3m 14s · ↓ 102.8k
    //     tokens`) — that agent's own token count.
    // Neither is the session's context size, which the status bar prints
    // bare (`224598 tokens`) with no arrow.
    //
    // The generic `token_re` above has no arrow anchor, so it happily matches
    // those counters too, and the per-line loop below keeps the LAST match in
    // the bottom-10 window. Roster rows are drawn AFTER the status bar, so a
    // roster row inside the window overwrites the real total with an agent's
    // count. It only bites while an agent's count is under 1000: at 1000+ the
    // roster prints `1.2k tokens`, which `token_re` cannot match (the `.`
    // breaks `(\d[\d,]*)\s+tok`), so the total silently comes back.
    //
    // Observed 2026-08-20: three times in seven minutes the session total
    // (169233, 178465, …) was replaced by a just-spawned agent's row
    // (119, 21, 39, 74 tokens). Downstream that reads as "tokens collapsed
    // from 169k to 119" — i.e. a context clear — and the daemon injected a
    // post-clear resume prompt into a session that had never been cleared.
    //
    // Fix: blank out arrow-prefixed counter segments before running the
    // generic status-bar match. The dedicated `token_thinking_re` pass below
    // still recovers a thinking-indicator count when the status bar carries
    // no total of its own, so no coverage is lost.
    let arrow_counter_re =
        Regex::new(r"[\u{2191}\u{2193}]\s*\d[\d,.]*\s*[kKmM]?\s*tok").unwrap();
    // Claude Code has used multiple names for the concurrent-task counter:
    // `bashes` (old), `background tasks` (mid), and `shells` (2.1.94+). Match
    // all of them — including the singular forms (status bar shows
    // "1 shell" / "1 bash", not "1 shells"). Also tolerate an optional
    // "active" qualifier inserted by the Background-tasks overlay panel
    // ("4 active shells", "1 active agent"). The negative lookahead-equivalent
    // here is the trailing whitespace/end check: `\b` after `(?:s)?` keeps
    // "5 shellscript" from matching.
    // NOTE: regex_lite quirk — `bashes?` does NOT match `bash` (only matches
    // `bashes`). We have to spell out the optional plural suffix as `(?:es)?`
    // / `(?:s)?` for it to behave correctly. Same trap applies to `tasks?`
    // and `shells?` — write them as `task(?:s)?` and `shell(?:s)?` to be
    // safe.
    //
    // `monitor(?:s)?` covers the Monitor-tool background-watch counter
    // Claude Code's status bar renders as `· 2 monitors ·` (2026-08-27
    // corruption-detector regression): it is a concurrent-task count exactly
    // like bashes/background-tasks/shells, but was missing from this
    // alternation, so a pane with live Monitor-tool watches and no other
    // running shell/task parsed `bashes == 0` — the FIRST domino in the
    // "tokens==0 && bashes==0 -> dead process" misfire (see
    // `pane_shows_active_ui` below for the second line of defense).
    let bash_re = Regex::new(
        r"(\d+)\s+(?:active\s+)?(?:bash(?:es)?|background\s+task(?:s)?|shell(?:s)?|monitor(?:s)?)\b",
    )
    .unwrap();
    let compact_re = Regex::new(r"Context left until auto-compact:\s*(\d+)%").unwrap();

    // Check if ANY line in the bottom section is a status bar line.
    // When the tmux pane is narrow, the status bar wraps across multiple lines —
    // e.g. "bypass permissions" on one line and "175630 tokens" on the next.
    // Narrow wrapping can ALSO split "bypass permissions" itself across a
    // separator ("bypass permissi ·  on"), so we match the more reliable prefix
    // "bypass permissi" instead of the full word.
    //
    // EXTREME wraps (2026-04-18 incident) split the bar across many logical
    // lines so even "bypass permissi" doesn't appear on any one line — just
    // `bypass` alone on its line, then `INSERT` alone, then `606746 tokens`
    // alone. The `⏵⏵` permission-mode icon is the most reliable anchor:
    // Claude Code emits it at the left edge of the status bar whenever
    // bypass or accept-edits permissions are active, and it never appears in
    // Claude's chat output or model responses. Match it first.
    //
    // If we see a status bar indicator anywhere, enable token parsing for all
    // lines in the tail.
    let is_status_bar_marker = |line: &str| -> bool {
        line.contains('\u{23f5}') // ⏵ — permission mode icon (bypass / accept edits)
            || line.contains("bypass permissi")
            || line.contains("-- INSERT --")
            || line.contains("background tasks")
            || line.contains("background task")
            || line.contains("bashes")
            || line.contains(" shells")
            || line.contains(" shell ")
            || line.contains("active shells")
            || line.contains("active agents")
            || line.contains("active agent")
            || line.contains("auto-compact")
    };

    let has_status_bar = lines[start..].iter().any(|l| is_status_bar_marker(l));

    for line in &lines[start..] {
        if has_status_bar {
            // Strip arrow-prefixed counters (thinking indicator, agent-roster
            // rows) so they cannot overwrite the status bar's bare total.
            let bare = arrow_counter_re.replace_all(line, " ");
            if let Some(caps) = token_re.captures(&bare) {
                if let Some(m) = caps.get(1) {
                    let cleaned = m.as_str().replace(',', "");
                    if let Ok(v) = cleaned.parse::<u64>() {
                        result.tokens = Some(v);
                    }
                }
            }
        }
        // Thinking-indicator lines (`↑/↓ N tokens`) and agent-roster rows
        // both carry a "N tokens" number, but NEITHER is the session
        // context total:
        //   * the thinking indicator's number is the CURRENT TURN's own
        //     streamed/output token count — it starts near zero and climbs
        //     for as long as that turn is generating;
        //   * a roster row's number belongs to ONE SUBAGENT.
        //
        // ROOT CAUSE (2026-08 recurring false "fresh session" fires): this
        // function used to fall back to whichever of those two numbers it
        // could find whenever the bare total wasn't on screen — reasoning
        // that a wrong-but-plausible number was "better than nothing" for
        // the rare case where an overlay panel had pushed the real total
        // off screen (PR #646, 2026-04-27). But the bare total is ALSO
        // absent every time the main loop is simply mid-turn — which is
        // routine, not rare — so on every such poll the fallback handed a
        // tiny, climbing "current turn" count to callers as if it were the
        // session's context size. Downstream consumers that watch for a
        // huge-to-tiny drop (context-clear / fresh-session detection) read
        // that as the context collapsing and injected a bogus "fresh
        // session, run the resume checklist" prompt every few minutes,
        // even though the real context was untouched.
        //
        // Fix: never let either number populate `result.tokens`. Still
        // record that we *saw* one, purely so `is_parse_miss` (below)
        // continues to treat a thinking/roster-only pane as a recognized
        // UI state rather than a suspicious miss.
        if is_agent_roster_row(line) {
            saw_agent_roster = true;
        } else if token_thinking_re.is_match(line) {
            saw_thinking_indicator = true;
        }
        if let Some(caps) = bash_re.captures(line) {
            if let Some(m) = caps.get(1) {
                if let Ok(v) = m.as_str().parse::<u64>() {
                    result.bashes = Some(v);
                }
            }
        }
        if let Some(caps) = compact_re.captures(line) {
            if let Some(m) = caps.get(1) {
                if let Ok(v) = m.as_str().parse::<u32>() {
                    result.compact_remaining = Some(v);
                }
            }
        }
    }

    // Overlay-fallback pass: when the inline status-bar pass fails to
    // extract counts but the FULL pane (not just the bottom 10 lines)
    // contains overlay markers OR a thinking-indicator token line, scan
    // the entire pane. This handles the "Background tasks" overlay
    // (2026-04-27 incident) where:
    //   - The overlay is taller than 10 lines (header + count + "Shells (N)"
    //     section + per-shell rows + "Local agents (N)" section + per-agent
    //     rows + nav-hint row), AND
    //   - tmux capture preserves blank lines that the parse_miss_tail
    //     diagnostic strips, so the WARN tail looks like the count line
    //     should have been visible — but the parser's bottom-10-line
    //     window had been pushed past it by intervening blanks.
    //
    // The thinking-indicator regex (↑/↓ N tokens) and the overlay's
    // "N active shells" / "N active agent" pattern are both unique enough
    // that scanning the whole pane is safe (no risk of matching prose).
    let overlay_visible = lines.iter().any(|line| {
        line.contains("Background tasks")
            || line.contains("active shells")
            || line.contains("active shell")
            || line.contains("active agents")
            || line.contains("active agent")
            || line.contains("Local agents")
            || line.starts_with("  Shells (")
            || line.contains(" Shells (")
    });

    if overlay_visible {
        // Whole-pane scan for bashes (overlay layout), but only if not
        // already found.
        if result.bashes.is_none() {
            for line in &lines {
                if let Some(caps) = bash_re.captures(line) {
                    if let Some(m) = caps.get(1) {
                        if let Ok(v) = m.as_str().parse::<u64>() {
                            result.bashes = Some(v);
                            break;
                        }
                    }
                }
            }
        }
    }

    // Whole-pane scan: a thinking-indicator or roster line can sit more
    // than 10 lines above the bottom (overlay panels push the tail up).
    // Same rule as the bottom-10 pass above: this only ever updates the
    // "did we see one" markers, never `result.tokens` — see the fix note
    // on the bottom-10 pass for why.
    if !saw_thinking_indicator || !saw_agent_roster {
        for line in &lines {
            if is_agent_roster_row(line) {
                saw_agent_roster = true;
            } else if token_thinking_re.is_match(line) {
                saw_thinking_indicator = true;
            }
        }
    }

    // Treat the overlay, and a thinking-indicator/roster sighting, as a
    // status-bar marker: even though none of them carries a trustworthy
    // token total, they're all known UI states, and we don't want
    // `is_parse_miss` to flag a pane that's simply mid-turn.
    let saw_status_bar = has_status_bar || overlay_visible || saw_thinking_indicator || saw_agent_roster;

    (result, saw_status_bar)
}

/// Pure function: determine whether a parse-bar result + pane capture
/// represents a suspicious "parse miss" — i.e. the pane had non-whitespace
/// content, NO status-bar marker, and we extracted neither tokens nor
/// bashes. This is the case we want to log loudly so we can diagnose
/// stale-latch bugs where the daemon repeatedly reads 0 from a pane that
/// clearly has a status bar.
///
/// Cases that are NOT parse misses:
///   1. Empty / all-whitespace capture → "process is actually gone"
///   2. Either tokens OR bashes successfully parsed
///   3. A status-bar marker (⏵⏵, INSERT, etc.) was visible AND a count
///      was extracted — handled by case 2.
///
/// Cases that ARE parse misses (warn so the parser can be hardened):
///   - Pane has content, no status-bar markers visible, no counts parsed.
///     This usually means an overlay panel obscured the bar AND the
///     thinking indicator token-count form is also unrecognised, OR a
///     new Claude Code UI variant has shipped that we don't recognise.
///
/// A status bar that IS visible but legitimately has no shell/token
/// counts (e.g. idle: `⏵⏵ bypass permissions on (shift+tab to cycle) ·
/// esc to interrupt` — 0 shells, 0 tokens because nothing's happening)
/// is NOT a parse miss. We saw the bar, the counts truly aren't present,
/// nothing to harden.
pub(crate) fn is_parse_miss(
    pane_text: &str,
    parsed: &ParsedStatusBar,
    saw_status_bar: bool,
) -> bool {
    if parsed.tokens.is_some() || parsed.bashes.is_some() {
        return false;
    }
    // Status bar visible but no counts — legitimately idle, not a miss.
    if saw_status_bar {
        return false;
    }
    pane_text.chars().any(|c| !c.is_whitespace())
}

/// Pure function: extract a short diagnostic tail from a pane capture for
/// logging. Returns the last `max_lines` non-empty lines, each truncated to
/// `max_line_len` characters. Keeps log volume bounded even if the pane has
/// huge lines.
pub(crate) fn parse_miss_tail(pane_text: &str, max_lines: usize, max_line_len: usize) -> String {
    let lines: Vec<&str> = pane_text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| {
            if line.chars().count() > max_line_len {
                let truncated: String = line.chars().take(max_line_len).collect();
                format!("{}…", truncated)
            } else {
                (*line).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Extract a version string from a path containing `/versions/X.Y.Z/`.
pub(crate) fn extract_version_from_path(path: &str) -> Option<String> {
    let re = Regex::new(r"/versions/([\d.]+)").unwrap();
    re.captures(path)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

/// Extract the `"version"` field from a Claude `package.json` / session-marker
/// JSON blob. Pure string parse (no `serde` round-trip) so it stays cheap and
/// tolerant of the surrounding fields varying between Claude releases.
pub(crate) fn extract_version_from_json(json: &str) -> Option<String> {
    let re = Regex::new(r#""version"\s*:\s*"([\d][\d.]*)""#).unwrap();
    re.captures(json)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

/// Resolve the on-disk (installed) Claude Code version, handling both install
/// layouts Claude itself ships:
///
/// 1. **Native versioned-symlink layout** (`installMethod: native`): the
///    `claude` launcher is a symlink into
///    `~/.local/share/claude/versions/X.Y.Z/...`, so the version is encoded in
///    the canonicalized path and `extract_version_from_path` recovers it.
/// 2. **npm-global layout** (`installMethod: global`): the launcher resolves to
///    `.../node_modules/@anthropic-ai/claude-code/bin/claude.exe`, which has NO
///    version in the path. The authoritative version is the `"version"` field
///    of that package's `package.json`. We walk up from the canonicalized
///    binary to the package root and read it.
///
/// `claude_bin` is the launcher path to canonicalize (e.g. the result of
/// resolving `claude` on `PATH`, or a well-known install location).
pub(crate) fn resolve_installed_version(claude_bin: &std::path::Path) -> Option<String> {
    let target = std::fs::canonicalize(claude_bin).ok()?;
    let target_str = target.to_string_lossy();

    // Layout 1: native versioned-symlink — version is in the path.
    if let Some(ver) = extract_version_from_path(&target_str) {
        return Some(ver);
    }

    // Layout 2: npm-global — walk up from the binary looking for the
    // package.json that carries the version. The canonical npm layout is
    // `<pkg>/bin/claude.exe` so the package.json is typically 1-2 levels up,
    // but bound the walk so a pathological symlink can't loop us forever.
    let mut dir = target.parent();
    for _ in 0..4 {
        let Some(d) = dir else { break };
        let pkg_json = d.join("package.json");
        if let Ok(contents) = std::fs::read_to_string(&pkg_json) {
            // Only trust a package.json that is actually the claude-code
            // package, so we don't accidentally read some unrelated
            // `bin/package.json` higher up the tree.
            if contents.contains("@anthropic-ai/claude-code") {
                if let Some(ver) = extract_version_from_json(&contents) {
                    return Some(ver);
                }
            }
        }
        dir = d.parent();
    }

    None
}

/// Returns true if `canonical` looks like the Claude Code MCP-settings hooks
/// shim (a bash wrapper) rather than the real CLI binary.
///
/// The container puts a shim FIRST on `PATH` (e.g.
/// `/usr/local/lib/claude-hooks-shim/claude` → `.../claude-mcp-settings-shim`,
/// a bash script) so `command -v claude` resolves to the wrapper, not the real
/// binary. Resolving the installed version off the shim yields `None` (no
/// `/versions/` segment, no `@anthropic-ai/claude-code/package.json` up the
/// tree). Mirror the shim's own self-skip logic: a canonical path containing
/// `hooks-shim` or `claude-mcp-settings-shim` is the wrapper.
fn is_claude_hooks_shim(canonical: &std::path::Path) -> bool {
    let s = canonical.to_string_lossy();
    s.contains("hooks-shim") || s.contains("claude-mcp-settings-shim")
}

/// Locate the `claude` launcher binary to inspect for the installed version.
///
/// Prefers the native versioned-symlink location (`~/.local/bin/claude`) when
/// present — it canonicalizes straight to a versioned path. Otherwise walks
/// `PATH` (like `which -a`) and returns the FIRST `claude` whose canonicalized
/// target is NOT the MCP-settings hooks shim. This is deliberately NOT
/// `command -v claude`: that returns whatever is first on `PATH`, which in the
/// container is the hooks-shim wrapper (`/usr/local/lib/claude-hooks-shim/claude`
/// → `claude-mcp-settings-shim`, a bash script) — canonicalizing it finds no
/// version, so `latest`/`installed` would read "unknown". Skipping the shim
/// (see [`is_claude_hooks_shim`]) returns the real binary
/// (`.../node_modules/@anthropic-ai/claude-code/bin/claude.exe`), off which the
/// walk-up in [`resolve_installed_version`] finds the package.json version.
fn find_claude_launcher() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());

    // Native layout first (cheap, deterministic).
    let native = std::path::PathBuf::from(format!("{home}/.local/bin/claude"));
    if native.exists() {
        return Some(native);
    }

    // Walk every PATH entry (like `which -a claude`) and return the first
    // `claude` that exists, is executable, and is NOT the hooks-shim wrapper.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':').filter(|d| !d.is_empty()) {
            let candidate = std::path::Path::new(dir).join("claude");
            if !is_executable_file(&candidate) {
                continue;
            }
            // Canonicalize to see through the shim symlink chain; skip the shim.
            match std::fs::canonicalize(&candidate) {
                Ok(canonical) if !is_claude_hooks_shim(&canonical) => {
                    return Some(candidate);
                }
                // Shim (or unresolvable) — keep looking down PATH.
                _ => continue,
            }
        }
    }

    None
}

/// Returns true if `path` exists, is a regular file (following symlinks), and
/// has any execute bit set. Used to mirror `which`'s "executable on PATH"
/// check while walking `PATH` entries.
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        // metadata follows symlinks, so a symlink → real binary is a file here.
        Ok(md) => md.is_file() && (md.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Resolve the running Claude Code version for a given PID.
///
/// 1. **Native layout**: `/proc/PID/exe` resolves to the versioned binary path,
///    so the version is recoverable directly from the link target.
/// 2. **npm-global layout**: `/proc/PID/exe` points at a now-deleted temp path
///    (npm's atomic install renames a `.claude-code-XXXX` staging dir over the
///    live package, leaving the running process mapped to the old, deleted
///    inode), so the path carries no usable version. Claude itself records the
///    running version in `~/.claude/sessions/<PID>.json` (the `"version"`
///    field), which is the authoritative "what THIS running PID loaded"
///    snapshot for every install method — read that.
///
/// These two layouts are the ONLY sources. We intentionally do NOT shell out to
/// `claude --version` as a fallback: that yields the INSTALLED version, not what
/// is running in this PID. On a deleted-inode exe link the truthful running
/// version is the OLD version this PID loaded (per the session marker) — never
/// the newly-installed one. If neither layout resolves, return `None`; we never
/// substitute the installed value for the running one. (The separate
/// `installed`/`latest` field IS sourced from the installed binary — see
/// [`find_claude_launcher`] + [`resolve_installed_version`].)
fn resolve_running_version(pid: &str) -> Option<String> {
    // Layout 1: versioned /proc/PID/exe target.
    let exe_path = format!("/proc/{pid}/exe");
    if let Ok(target) = std::fs::read_link(&exe_path) {
        if let Some(ver) = extract_version_from_path(&target.to_string_lossy()) {
            return Some(ver);
        }
    }

    // Layout 2 (terminal fallback): Claude's own per-PID session marker. This
    // is the authoritative "what THIS running PID loaded" snapshot, written at
    // process start, for every install method.
    //
    // We deliberately STOP here. We do NOT shell out to `claude --version` as a
    // further fallback: that reports the INSTALLED version, not what is actually
    // running in this PID. When `/proc/PID/exe` is a deleted inode (npm's atomic
    // install renamed a newer package over the live one) the truthful running
    // version is the OLD version this PID loaded — recorded in the session
    // marker — NOT the freshly-installed one. Reporting the installed version
    // here would lie about what is in-process. If neither the exe link nor the
    // marker yields a version we return `None` rather than "freshen" it with the
    // installed value. (The `installed`/`latest` field is sourced separately via
    // find_claude_launcher() + resolve_installed_version(), where the installed
    // binary IS the right answer.)
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    let session_marker = format!("{home}/.claude/sessions/{pid}.json");
    if let Ok(contents) = std::fs::read_to_string(&session_marker) {
        if let Some(ver) = extract_version_from_json(&contents) {
            return Some(ver);
        }
    }

    None
}

/// Get installed and running Claude Code versions.
///
/// Handles both Claude install layouts (native versioned-symlink and
/// npm-global) generically — see [`resolve_installed_version`] and
/// [`resolve_running_version`]. This matters for restart-nudge detection: when
/// the running session is behind an already-on-disk newer build (e.g. running
/// 2.1.175 while 2.1.178 is installed), both fields must resolve so the
/// auto-update policy can detect the mismatch instead of bailing on `None`.
pub fn get_version_info() -> VersionInfo {
    let mut info = VersionInfo::default();

    // Installed (on-disk) version.
    if let Some(bin) = find_claude_launcher() {
        info.installed = resolve_installed_version(&bin);
    }

    // Running version: iterate claude PIDs, take the first that resolves.
    //
    // Match on the FULL command line (`pgrep -af`), NOT the process name
    // (`pgrep -a`, which matches `comm`). The claude-code NATIVE installer runs
    // the inner claude as `~/.local/share/claude/versions/<X.Y.Z>`, so on those
    // builds the process `comm` is the bare VERSION STRING (e.g. `2.1.235`), not
    // `claude` — the documented native-installer gotcha that already broke pane
    // detection (see `find_claude_pane` / `is_version_string_comm`). A
    // comm-based `pgrep claude` MISSES that PID entirely (it matches only
    // `claude-watch`, which never resolves to a running version), so `running`
    // came back `None` and the version panel reported `current=unknown` while
    // `latest`/`installed` (sourced from the versions-dir symlink) resolved
    // fine. The binary PATH always contains `claude`
    // (`.local/bin/claude` or `.local/share/claude/versions/...`), so a
    // full-cmdline match finds the PID regardless of how it retitled its `comm`.
    if let Ok(output) = std::process::Command::new("pgrep")
        .args(["-af", "claude"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(pid_str) = line.split_whitespace().next() {
                if let Some(ver) = resolve_running_version(pid_str) {
                    info.running = Some(ver);
                    break;
                }
            }
        }
    }

    info
}

/// Resolve a tmux pane's foreground PID — the process tmux forked for the pane.
/// In the main-loop pane that is the claude TUI itself (launched as
/// `exec claude`), so its `/proc/<pid>/exe` points straight at the running
/// versioned binary.
async fn resolve_pane_pid(pane: &str) -> Option<String> {
    let (out, ok) = run_cmd_any(
        &["tmux", "display-message", "-p", "-t", pane, "#{pane_pid}"],
        5,
    )
    .await;
    if !ok {
        return None;
    }
    let pid = out.trim();
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(pid.to_string())
}

/// True iff an exe path looks like a REAL claude TUI binary — the native
/// versioned layout (under a `/versions/` dir), the `~/.local/bin/claude`
/// launcher, or an npm-global `@anthropic-ai/claude-code` binary — as opposed
/// to tmux, `claude-watch`, or a bash watcher that merely matched
/// `pgrep -af claude` on its command line. Used to filter the fallback
/// candidate set so a non-claude PID can never contribute a bogus version.
pub(crate) fn is_claude_tui_exe(exe_path: &str) -> bool {
    exe_path.contains("/versions/")
        || exe_path.ends_with("/.local/bin/claude")
        || exe_path.contains("@anthropic-ai/claude-code")
}

/// Compare two dotted numeric version strings (`"2.1.245"` vs `"2.1.243"`).
/// Components parse as integers (non-numeric / missing components sort as 0),
/// so `2.1.245 > 2.1.99` (numeric, not lexical).
pub(crate) fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> Vec<u64> {
        s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
    }
    parts(a).cmp(&parts(b))
}

/// A resolved running-claude candidate: a PID and the version it loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningCandidate {
    pub pid: String,
    pub version: String,
}

/// Pure selection core for pane-scoped running-version detection.
///
/// Given the resolved running-claude candidates and the main-loop pane's PID,
/// pick the running version: PREFER the candidate whose PID is the pane PID
/// (the live main-loop TUI); otherwise fall back to the HIGHEST version among
/// candidates. The highest-wins fallback guarantees that a dying OLDER
/// versioned process — a native/npm atomic-install overlap, or a
/// SIGKILL-orphaned old build still executable on disk — can NEVER mask the
/// live newer one, which is exactly the false `running < installed` mismatch
/// that drove the self-sustaining auto-update relaunch loop.
pub(crate) fn select_running_version(
    candidates: &[RunningCandidate],
    pane_pid: Option<&str>,
) -> Option<String> {
    if let Some(pp) = pane_pid {
        if let Some(c) = candidates.iter().find(|c| c.pid == pp) {
            return Some(c.version.clone());
        }
    }
    candidates
        .iter()
        .max_by(|a, b| compare_versions(&a.version, &b.version))
        .map(|c| c.version.clone())
}

/// Gather resolved running-claude candidates for the fallback path: every
/// `pgrep -af claude` PID whose `/proc/<pid>/exe` is a real claude TUI binary
/// (see [`is_claude_tui_exe`]) and that resolves to a version. Non-claude
/// matches (tmux/`claude-watch`/bash watchers) are filtered out so they cannot
/// contribute a bogus version.
async fn scan_running_candidates() -> Vec<RunningCandidate> {
    let (stdout, _) = run_cmd_any(&["pgrep", "-af", "claude"], 5).await;
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(pid) = line.split_whitespace().next() else {
            continue;
        };
        // Filter to real claude TUI processes by their exe target.
        let exe_ok = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|t| is_claude_tui_exe(&t.to_string_lossy()))
            .unwrap_or(false);
        if !exe_ok {
            continue;
        }
        if let Some(version) = resolve_running_version(pid) {
            out.push(RunningCandidate {
                pid: pid.to_string(),
                version,
            });
        }
    }
    out
}

/// Pane-scoped variant of [`get_version_info`]: resolve the RUNNING version
/// from the main-loop pane's own PID rather than the global `pgrep -af claude`
/// first-match.
///
/// ## Why (the auto-update version-detection loop)
///
/// [`get_version_info`] takes the FIRST `pgrep -af claude` match that resolves
/// to a version. In a container that pattern matches ~10+ PIDs (the tmux
/// launcher, `tmux attach`, `claude-watch`, bash watchers) and — critically —
/// any SIGKILL-orphaned OLD versioned claude processes that briefly coexist
/// with the live one during a respawn-pane `-k` overlap (the versions dir keeps
/// several builds all executable). When an older PID sorts first, `running`
/// comes back OLDER than `installed` (the symlink, always newest), so
/// `check_auto_update` sees a false `running != installed`, fires
/// `cwsr --no-upgrade` (which swaps nothing on disk), and re-fires next cycle —
/// self-sustaining. Pane DETECTION already dodges this exact first-match hazard
/// via [`find_claude_pane_with_config`]; version detection never did.
///
/// Resolution:
///   1. Resolve the pane's `#{pane_pid}` (the live claude TUI) and read the
///      running version from THAT PID only.
///   2. If the pane PID can't be resolved or yields no version, fall back to a
///      FILTERED scan (real claude TUI exes only) and pick the HIGHEST version
///      (see [`select_running_version`]) so a dying old PID can't win.
///
/// `installed` is sourced identically to [`get_version_info`].
pub async fn get_version_info_for_pane(pane: &str) -> VersionInfo {
    let mut info = VersionInfo::default();

    // Installed (on-disk) version — identical source to get_version_info.
    if let Some(bin) = find_claude_launcher() {
        info.installed = resolve_installed_version(&bin);
    }

    let pane_pid = resolve_pane_pid(pane).await;

    // Primary: the version loaded by the pane's own PID.
    if let Some(pid) = pane_pid.as_deref() {
        info.running = resolve_running_version(pid);
    }

    // Fallback (pane PID unresolved, or it carried no version): filtered scan,
    // preferring the pane PID, else the highest version among real claude TUIs.
    if info.running.is_none() {
        let candidates = scan_running_candidates().await;
        info.running = select_running_version(&candidates, pane_pid.as_deref());
    }

    info
}

/// Find the MAIN-LOOP Claude Code pane, preferring the explicitly-configured
/// `[tmux] dashboard_pane` / `dashboard_session` over the unconstrained
/// auto-detect scan.
///
/// ## Why this exists (the focus-follows-inject bug)
///
/// The bare `find_claude_pane()` scans `tmux list-panes -a` and returns the
/// FIRST pane whose `pane_current_command == "claude"`. In a single-claude
/// layout that is always the main loop. But Claude Code's TUI agent-view
/// (the operator focusing a running SUBAGENT in the curses panel) spawns a
/// SECOND `claude` process in its own pane. With two `claude` panes present,
/// `find_claude_pane()`'s "first match wins" is order-dependent and can
/// resolve to the SUBAGENT's pane — so the daemon's MAIN-LOOP-SCOPED injects
/// (watcher-down restart, heartbeat-stale nudge, resume) land in the
/// subagent's context. That is pure noise there: a subagent cannot restart a
/// watcher or act on a heartbeat tick, the inject pollutes its context and
/// burns its tokens, and the main loop never sees the alert it must act on.
///
/// The daemon ALREADY knows the fixed main-loop pane — the in-container config
/// pins `dashboard_pane = "claude-container:0.0"` / `dashboard_session =
/// "claude-container"`. `send-keys` to a specific pane id works regardless of
/// which pane the operator has focused, so targeting the configured pane
/// EXPLICITLY (never the active/first-scanned pane) is both correct and
/// focus-independent. There is no legitimate case for a watcher-down /
/// heartbeat inject to target the active pane — the main loop is always the
/// configured pane.
///
/// Resolution order:
///   1. If `dashboard_pane` / `dashboard_session` is configured (non-empty),
///      resolve via `tmux::find_dashboard_pane` (config-first, status-bar
///      independent). This is the fixed main-loop pane and is returned even
///      when the operator has an agent-view pane focused.
///   2. Otherwise (unconfigured — fresh install / host dev), fall back to the
///      historical `find_claude_pane()` auto-detect scan.
pub async fn find_claude_pane_with_config(config: &crate::config::TmuxConfig) -> Option<String> {
    // Config-first: an explicitly-configured pane/session is the fixed
    // main-loop target. `find_dashboard_pane` checks `has-session` +
    // `display-message` on the configured pane before returning it, so a
    // configured-but-gone pane correctly falls through to the scan below.
    if prefer_configured_pane(config) {
        if let Some(p) = crate::tmux::find_dashboard_pane(config).await {
            debug!(pane = %p, "resolved main-loop pane via configured dashboard_pane/session");
            return Some(p);
        }
        debug!(
            session = %config.dashboard_session,
            "configured dashboard_session set but pane unresolved; falling back to auto-detect scan"
        );
    }
    find_claude_pane().await
}

/// Pure predicate: should pane resolution PREFER the explicitly-configured
/// `[tmux]` pane/session over the unconstrained auto-detect scan?
///
/// True iff a `dashboard_session` is configured (non-empty). When set, the
/// daemon has been told exactly which session hosts the main loop, so the
/// fixed configured pane is the correct inject target even if the operator
/// has a TUI agent-view subagent pane focused. When unset (fresh install /
/// host dev), there is nothing to prefer — fall back to the scan.
///
/// Factored out as a pure fn so the config-branch decision is unit-testable
/// without a live tmux (the actual pane lookup shells out and is covered by
/// the live-tmux e2e test).
pub(crate) fn prefer_configured_pane(config: &crate::config::TmuxConfig) -> bool {
    !config.dashboard_session.is_empty()
}

/// True iff `comm` looks like a bare semver version string (`<n>.<n>.<n>`,
/// optionally with more dot-separated numeric components), i.e. the tmux
/// `pane_current_command` a NATIVE-installer claude presents (running as
/// `~/.local/share/claude/versions/2.1.217`, tmux shows the basename
/// `2.1.217`). Purely digits-and-dots with at least two dots keeps it from
/// matching ordinary binary names.
pub(crate) fn is_version_string_comm(comm: &str) -> bool {
    let parts: Vec<&str> = comm.split('.').collect();
    parts.len() >= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Find the tmux pane running Claude Code.
///
/// Primary: look for `pane_current_command == "claude"` (legacy npm install)
/// OR a bare semver version string like `2.1.217` (the NATIVE installer — see
/// the comm-name note below).
/// Fallback: content-check EVERY pane for Claude Code status-bar text.
///
/// ## Command-name independence (native-installer regression, PR #473 / c7ee999)
///
/// The claude-code NATIVE installer runs claude as
/// `~/.local/share/claude/versions/<X.Y.Z>`, so a pane's
/// `pane_current_command` is the VERSION STRING (e.g. `2.1.217`), NOT
/// `claude`/`node`/`bash`. The old allow-list (`claude`, then only
/// `bash`/`node` as content candidates) MISSED entirely on native-install
/// hosts: `status --json` emitted no `pane`/`tokens` field, and `self-clear`'s
/// pane detection failed so context-exhausted sessions never auto-recovered.
/// Fix: accept a semver-shaped comm as a fast-path, and treat EVERY pane
/// (regardless of comm) as a content-check candidate.
///
/// NOTE: this is the CONFIG-IGNORANT auto-detect scan — it returns the FIRST
/// matching pane in tmux's list, which is order/focus-dependent when more than
/// one Claude Code pane exists (e.g. the operator focusing a TUI agent-view
/// subagent). Daemon paths that inject MAIN-LOOP-SCOPED keystrokes must prefer
/// `find_claude_pane_with_config` so the inject targets the configured fixed
/// main-loop pane, never whichever pane happens to sort first. See
/// `find_claude_pane_with_config` for the focus-follows-inject bug it fixes.
pub async fn find_claude_pane() -> Option<String> {
    let out = run_cmd(
        &[
            "tmux",
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_current_command}",
        ],
        5,
    )
    .await?;

    let mut candidates = Vec::new();

    for line in out.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 {
            // Unambiguous comm wins immediately: the legacy npm `claude`
            // command, or the native installer's bare `<major>.<minor>.<patch>`
            // version-string comm.
            if parts[1] == "claude" || is_version_string_comm(parts[1]) {
                return Some(parts[0].to_string());
            }
            // Every other pane is a content-check candidate — do NOT gate on
            // comm name (a wrapper shell or an unexpected comm must not be
            // excluded before the status-bar content check runs).
            candidates.push(parts[0].to_string());
        }
    }

    // Fallback: capture candidate panes and check for Claude Code status bar.
    //
    // Use joined capture (-J) so wrapped status bar lines reassemble into one
    // line — narrow panes wrap and truncate, but -J gives us the full logical
    // line before terminal truncation.
    //
    // Match on "tok" (not "tokens") because Claude Code truncates the status
    // bar with an ellipsis when the pane is narrow, producing things like
    // `502064 tok…`. Similarly, "bypass permissi" covers truncated
    // "bypass permissions". Also accept "background tasks" / "bashes" as
    // status-bar indicators (already used by parse_status_bar).
    //
    // The status bar format varies — sometimes shows "auto-compact", "latest",
    // "current:", or just "N tok…". Use multiple heuristics.
    for pane in candidates {
        let content = match crate::tmux::capture_pane_joined(&pane).await {
            Some(c) => Some(c),
            None => crate::tmux::capture_pane(&pane).await,
        };
        if let Some(content) = content {
            if content.contains("tok")
                && (content.contains("auto-compact")
                    || content.contains("latest")
                    || content.contains("current:")
                    || content.contains("❯")
                    || content.contains("-- INSERT --")
                    || content.contains("bypass permissi")
                    || content.contains("background tasks")
                    || content.contains("bashes")
                    || content.contains(" shells"))
            {
                return Some(pane);
            }
        }
    }

    None
}

/// Get Claude Code status by natively finding the pane, parsing the status bar,
/// and reading version info from /proc.
///
/// CONFIG-IGNORANT convenience shim: resolves the pane via the bare
/// auto-detect scan (`find_claude_pane`). Use `get_claude_status_with_config`
/// from any path that injects MAIN-LOOP-SCOPED keystrokes (the daemon
/// `check_cycle`) so the returned `pane` is the configured fixed main-loop
/// pane rather than whichever `claude` pane sorts first — see
/// `find_claude_pane_with_config`. Non-injecting callers (metrics exporter,
/// hook context probe, `claude-watch status`) keep using this no-arg form:
/// they only READ token/bash counts off the pane, where the multi-pane
/// ambiguity is harmless.
///
/// Falls back to shelling out to `claude-status --json` if native pane discovery
/// fails or if `CLAUDE_STATUS_CMD` env var is set (for test environments).
pub async fn get_claude_status() -> Option<ClaudeStatus> {
    get_claude_status_inner(None).await
}

/// Config-aware variant of [`get_claude_status`]. Resolves the pane via
/// [`find_claude_pane_with_config`], so when `[tmux] dashboard_pane` /
/// `dashboard_session` is configured the status (and crucially the `pane`
/// field the daemon then injects into) is pinned to the fixed main-loop pane,
/// never an operator-focused TUI agent-view subagent pane.
pub async fn get_claude_status_with_config(
    config: &crate::config::TmuxConfig,
) -> Option<ClaudeStatus> {
    get_claude_status_inner(Some(config)).await
}

async fn get_claude_status_inner(
    config: Option<&crate::config::TmuxConfig>,
) -> Option<ClaudeStatus> {
    // If CLAUDE_STATUS_CMD is set (test mode), skip native discovery and use fallback
    if std::env::var("CLAUDE_STATUS_CMD").is_ok() {
        debug!("CLAUDE_STATUS_CMD set, using fallback");
        return get_claude_status_fallback().await;
    }

    // Try native pane discovery first. Prefer the configured fixed main-loop
    // pane when a config was threaded through (daemon path); otherwise the
    // historical auto-detect scan.
    let discovered = match config {
        Some(cfg) => find_claude_pane_with_config(cfg).await,
        None => find_claude_pane().await,
    };
    if let Some(pane) = discovered {
        debug!(pane = %pane, "found claude pane (native)");

        // Use joined capture (-J) for status bar parsing to avoid truncation
        if let Some(capture) = crate::tmux::capture_pane_joined(&pane).await {
            let (parsed, saw_status_bar) = parse_status_bar_with_diag(&capture);

            // Diagnostic: if we got nothing out of the parser but the pane
            // clearly has content AND no status bar was visible at all, log
            // the tail so we can debug stale-latch bugs where the daemon
            // reads tokens=0 forever while the CLI parses the same pane
            // correctly. A status bar that IS visible but has no counts
            // (legitimately idle) is not a miss.
            if is_parse_miss(&capture, &parsed, saw_status_bar) {
                warn!(
                    pane = %pane,
                    tail = %parse_miss_tail(&capture, 10, 200),
                    "status parse miss: pane non-empty but no tokens/bashes extracted"
                );
            }

            let version_info = tokio::task::spawn_blocking(get_version_info)
                .await
                .unwrap_or_default();

            let status = ClaudeStatus {
                pane,
                tokens: parsed.tokens.unwrap_or(0),
                bashes: parsed.bashes.unwrap_or(0),
                active_ui: pane_shows_active_ui(&capture),
                compact_remaining: parsed.compact_remaining,
                version: version_info.running,
                latest: version_info.installed,
            };

            debug!(
                tokens = status.tokens,
                bashes = status.bashes,
                pane = %status.pane,
                compact_remaining = ?status.compact_remaining,
                version = ?status.version,
                latest = ?status.latest,
                "parsed claude status (native)"
            );

            return Some(status);
        }
    }

    // Fallback: shell out to claude-status --json (for test environments with mocks)
    debug!("native pane discovery failed, trying claude-status fallback");
    get_claude_status_fallback().await
}

/// Fallback: shell out to `claude-status --json` for status.
/// Used when native pane discovery fails (e.g. test environments with mock scripts).
async fn get_claude_status_fallback() -> Option<ClaudeStatus> {
    let out = run_cmd(&["claude-status", "--json"], 15).await?;
    debug!(raw_output = %out, "claude-status fallback response");
    let data: serde_json::Value = serde_json::from_str(&out).ok()?;

    let status = ClaudeStatus {
        pane: data["pane"].as_str().unwrap_or("").to_string(),
        tokens: data["tokens"].as_u64().unwrap_or(0),
        bashes: data["bashes"].as_u64().unwrap_or(0),
        active_ui: data["active_ui"].as_bool().unwrap_or(false),
        compact_remaining: data["compact_remaining"].as_u64().map(|v| v as u32),
        version: data["version"].as_str().map(|s| s.to_string()),
        latest: data["latest"].as_str().map(|s| s.to_string()),
    };
    debug!(tokens = status.tokens, bashes = status.bashes, pane = %status.pane, "parsed claude status (fallback)");
    Some(status)
}

pub async fn check_watchmen_count() -> u32 {
    let (out, _) = run_cmd_any(&["pgrep", "-fc", "bin/watchmen"], 5).await;
    out.parse().unwrap_or(0)
}

/// Count processes matching `pattern` via `pgrep -fc`.
///
/// Retained as a public primitive (parallel to [`check_watchmen_count`]).
/// The watcher-health monitor now prefers [`check_process_pids`] so it can
/// probe each matched PID for genuine liveness rather than trusting a raw
/// count (which includes zombies).
#[allow(dead_code)]
pub async fn check_process_count(pattern: &str) -> u32 {
    // Use "--" to prevent pgrep from interpreting patterns starting with "--" as options
    let (out, _) = run_cmd_any(&["pgrep", "-fc", "--", pattern], 5).await;
    out.parse().unwrap_or(0)
}

/// Return the PIDs of processes matching `pattern` via `pgrep -f`.
///
/// Unlike [`check_process_count`] (which only returns a count), this exposes
/// the individual PIDs so the caller can probe each for genuine liveness
/// (rejecting zombies / `<defunct>` entries that `pgrep` still counts because
/// they linger in the process table until reaped).
///
/// NOTE (2026-06-11): the watcher-health monitor NO LONGER uses this for
/// liveness. `pgrep -f <pattern>` is defeated when the watcher's launcher
/// script `exec`s a binary (the `.sh` pattern disappears from argv), causing
/// false `WATCHER(S) DOWN` alerts. The monitor now reads the watcher's own
/// pidfile/lockfile instead (see `policy::pidfile_watcher_is_down`). Retained
/// as a public primitive for any other caller that needs a pattern → PIDs
/// lookup.
#[allow(dead_code)]
pub async fn check_process_pids(pattern: &str) -> Vec<u32> {
    let (out, _) = run_cmd_any(&["pgrep", "-f", "--", pattern], 5).await;
    out.lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Parse watchers config file. Format:
/// `name|pattern|min_count|enabled|start_cmd[|on_restart_cmd]`
pub fn parse_watchers_config(path: &str) -> Vec<WatcherEntry> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    parse_watchers_config_str(&content)
}

/// The per-watcher fields a conf line may set, in POSITIONAL order (the
/// pipe-separated slot after `name`) — and the key each accepts in the
/// `key=value` form. Index = positional slot.
pub const WATCHER_FIELD_KEYS: [&str; 7] = [
    "pattern",
    "min_count",
    "enabled",
    "start_cmd",
    "on_restart_cmd",
    "mode",
    "monitor_cmd",
];

/// One conf line, parsed but not yet resolved: `fields[i]` is `Some(text)`
/// when the line SET slot `i` (see [`WATCHER_FIELD_KEYS`]) and `None` when
/// it left it blank / omitted it. Keeping "unset" distinct from "default" is
/// what lets the same line grammar serve both layers: in the base file an
/// unset field takes the documented default, in the override file it means
/// "inherit from base".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawWatcherLine {
    pub name: String,
    pub fields: [Option<String>; 7],
}

/// Parse one non-comment conf line. Grammar (both forms may mix on a line):
///
/// ```text
/// name|pattern|min_count|enabled|start_cmd|on_restart_cmd|mode|monitor_cmd   (positional)
/// name|mode=monitor|enabled=false                                          (keyed)
/// ```
///
/// A field whose text is `<known-key>=<value>` is KEYED and sets that key
/// regardless of its position; any other field is positional by its slot.
/// Blank positional fields are "unset". Lines with only a name (no `|`) are
/// rejected, as before.
pub(crate) fn parse_watcher_line(line: &str) -> Option<RawWatcherLine> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 2 {
        return None;
    }
    let name = parts[0].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut fields: [Option<String>; 7] = Default::default();
    for (slot, part) in parts[1..].iter().enumerate() {
        let text = part.trim();
        if let Some((k, v)) = text.split_once('=') {
            if let Some(idx) = WATCHER_FIELD_KEYS.iter().position(|key| *key == k.trim()) {
                fields[idx] = Some(v.trim().to_string());
                continue;
            }
        }
        if slot < WATCHER_FIELD_KEYS.len() && !text.is_empty() {
            fields[slot] = Some(text.to_string());
        }
    }
    Some(RawWatcherLine { name, fields })
}

/// Parse every non-comment, non-blank line of a conf file into raw lines.
pub(crate) fn parse_watcher_lines(content: &str) -> Vec<RawWatcherLine> {
    content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(parse_watcher_line)
        .collect()
}

/// Resolve a raw BASE-layer line into an entry, applying the documented
/// defaults for every unset field (`min_count` 1, `enabled` true, `mode`
/// oneshot, everything else empty).
pub(crate) fn entry_from_raw(raw: &RawWatcherLine, layer: &str) -> WatcherEntry {
    let f = &raw.fields;
    let nonempty = |i: usize| {
        f[i].as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    WatcherEntry {
        name: raw.name.clone(),
        pattern: f[0].clone().unwrap_or_default(),
        min_count: f[1]
            .as_deref()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1),
        enabled: f[2].as_deref().map(|s| s.trim() == "true").unwrap_or(true),
        start_cmd: nonempty(3),
        on_restart_cmd: nonempty(4),
        mode: f[5]
            .as_deref()
            .and_then(WatcherMode::parse)
            .unwrap_or_default(),
        monitor_cmd: nonempty(6),
        layer: layer.to_string(),
        overridden: Vec::new(),
    }
}

/// Apply one override line to an existing entry: only the fields the line
/// SET are changed; each changed field's key is recorded in `overridden`.
pub(crate) fn apply_override(entry: &mut WatcherEntry, raw: &RawWatcherLine) {
    fn note(entry: &mut WatcherEntry, key: &str) {
        if !entry.overridden.iter().any(|k| k == key) {
            entry.overridden.push(key.to_string());
        }
    }
    if let Some(v) = raw.fields[0].as_deref() {
        entry.pattern = v.to_string();
        note(entry, "pattern");
    }
    if let Some(v) = raw.fields[1].as_deref() {
        if let Ok(n) = v.trim().parse::<u32>() {
            entry.min_count = n;
            note(entry, "min_count");
        }
    }
    if let Some(v) = raw.fields[2].as_deref() {
        entry.enabled = v.trim() == "true";
        note(entry, "enabled");
    }
    if let Some(v) = raw.fields[3].as_deref() {
        entry.start_cmd = Some(v.to_string()).filter(|s| !s.is_empty());
        note(entry, "start_cmd");
    }
    if let Some(v) = raw.fields[4].as_deref() {
        entry.on_restart_cmd = Some(v.to_string()).filter(|s| !s.is_empty());
        note(entry, "on_restart_cmd");
    }
    if let Some(v) = raw.fields[5].as_deref() {
        if let Some(m) = WatcherMode::parse(v) {
            entry.mode = m;
            note(entry, "mode");
        }
    }
    if let Some(v) = raw.fields[6].as_deref() {
        entry.monitor_cmd = Some(v.to_string()).filter(|s| !s.is_empty());
        note(entry, "monitor_cmd");
    }
}

/// Pure function: parse a BASE-layer watchers config from a string.
pub(crate) fn parse_watchers_config_str(content: &str) -> Vec<WatcherEntry> {
    parse_watcher_lines(content)
        .iter()
        .map(|raw| entry_from_raw(raw, WATCHER_LAYER_BASE))
        .collect()
}

/// Pure function: merge an OVERRIDE-layer config string onto already-parsed
/// base entries. A line naming an existing watcher changes only the fields
/// it sets (blank = inherit); a line naming an unknown watcher is appended
/// as a new entry (layer `"override"`). Later lines win over earlier ones.
pub(crate) fn merge_watchers_override_str(
    mut base: Vec<WatcherEntry>,
    override_content: &str,
) -> Vec<WatcherEntry> {
    for raw in parse_watcher_lines(override_content) {
        if let Some(existing) = base.iter_mut().find(|e| e.name == raw.name) {
            apply_override(existing, &raw);
        } else {
            base.push(entry_from_raw(&raw, WATCHER_LAYER_OVERRIDE));
        }
    }
    base
}

/// Load the LAYERED watcher config: the base file plus an optional override
/// file (a user-dir file, typically a symlink into a dotfiles/config repo).
/// A missing base yields no entries (as before); a missing / unreadable
/// override is silently a no-op, so the committed default always loads on
/// its own. Symlinks are followed (`std::fs::read_to_string`), which is what
/// lets the override file be a symlink into a repo — with the caveat that
/// inside a container the symlink's TARGET must also be inside a mounted
/// tree, or the link dangles and the override is treated as absent.
///
/// This is THE loader both the CLI (`watcher-ctl`) and the daemon's
/// `watcher_monitor` use, so "what is enabled / which mode" can never
/// disagree between the two.
pub fn load_watchers_config(base_path: &str, override_path: Option<&str>) -> Vec<WatcherEntry> {
    let base = parse_watchers_config(base_path);
    match override_path.and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(content) => merge_watchers_override_str(base, &content),
        None => base,
    }
}

// ---------------------------------------------------------------------------
// Shared watcher-liveness helpers (single source of truth).
//
// These were originally private to `policy.rs` (the daemon's watcher_monitor,
// migrated to pidfile-based liveness in the 2026-06-11 exec-defeats-pgrep fix,
// PR #339). The parallel `watcher.rs` CLI status path (`watcher_status`) was
// left on the broken `pgrep -f <launcher.sh>` approach and therefore reported a
// healthy watcher as DOWN (the launcher `exec`s the bare binary, so the live
// argv no longer contains the `.sh` path). Hoisting the helpers here lets BOTH
// the daemon (`policy.rs`) AND the CLI (`watcher.rs`) decide UP/DOWN from the
// SAME pidfile-liveness logic, so they can never disagree again.
// ---------------------------------------------------------------------------

/// Check if a PID is still alive (signal-0 probe via SIGCONT delivery test).
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), Signal::SIGCONT)
        .map(|_| true)
        .unwrap_or(false)
}

/// Check if a PID is genuinely alive — i.e. exists AND is not a zombie
/// (`<defunct>`). `pgrep` still lists zombies because they linger in the
/// process table until reaped, so a plain `kill -0` probe (or a raw `pgrep`
/// count) would treat a defunct watcher as "running". We read `/proc/PID/stat`
/// and reject state `Z`/`X` so a watcher whose process has died-but-not-yet-
/// reaped is correctly seen as not-alive.
///
/// Falls back to the signal-0 probe when `/proc/PID/stat` is unreadable (e.g.
/// a non-Linux test host) so behaviour degrades to "exists?" rather than
/// always-false.
pub(crate) fn is_pid_genuinely_alive(pid: u32) -> bool {
    let path = format!("/proc/{}/stat", pid);
    match std::fs::read_to_string(&path) {
        Ok(stat) => {
            // /proc/PID/stat: `pid (comm) STATE ...`. comm can contain spaces
            // and parens, so find the LAST ')' and take the next token.
            if let Some(close) = stat.rfind(')') {
                let rest = stat[close + 1..].trim_start();
                let state = rest.split_whitespace().next().unwrap_or("");
                // 'Z' = zombie/defunct, 'X'/'x' = dead. Anything else is a
                // live, reapable-or-running process.
                return state != "Z" && state != "X" && state != "x";
            }
            // Malformed stat — fall back to existence probe.
            is_pid_alive(pid)
        }
        // No /proc entry (already reaped) or non-Linux host: fall back to the
        // signal probe.
        Err(_) => is_pid_alive(pid),
    }
}

/// Read `/proc/<pid>/cmdline` (NUL-separated argv) into a space-joined string.
/// Returns `None` if the process is gone, the file is unreadable, or the
/// cmdline is empty (e.g. a kernel thread). Used for watcher identity checks.
pub(crate) fn read_proc_cmdline(pid: u32) -> Option<String> {
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

/// Resolve the directory that holds watcher PID / lock files.
///
/// Mirrors the watcher's own lockfile resolution
/// (`$XDG_RUNTIME_DIR/<name>.lock` else `/var/run/claude/<name>.lock`) and
/// `watcher::pid_dir()` (`$CLAUDE_WATCH_PID_DIR` else `/var/run/claude`), so
/// both the daemon AND the CLI read the SAME file the watcher writes.
/// Precedence:
///   1. `$CLAUDE_WATCH_PID_DIR` (explicit override; used by tests + the
///      watcher_run spawn path).
///   2. `$XDG_RUNTIME_DIR` (matches the watcher's lockfile default).
///   3. `/var/run/claude` (final fallback — the baked container path).
///
/// NOTE: this single-dir resolver is INHERENTLY env-dependent and returns just
/// ONE directory, so it misses a watcher whose liveness file landed in a
/// different candidate (the `signal-wait` `.pid` in `/var/run/claude` vs a
/// reader whose `$XDG_RUNTIME_DIR` picked `/run/user/<uid>`). Production
/// detection now uses [`watcher_pid_dirs`] + [`watcher_pidfile_liveness_multi`],
/// which scan ALL candidates. Retained (with tests) as the primitive that
/// documents the precedence.
// dead_code allow: only test callers remain, invisible to the lib-only pass.
#[allow(dead_code)]
pub(crate) fn watcher_pid_dir() -> String {
    if let Ok(p) = std::env::var("CLAUDE_WATCH_PID_DIR") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    "/var/run/claude".to_string()
}

/// Pure candidate-directory resolver: given the (optional) values of
/// `$CLAUDE_WATCH_PID_DIR` and `$XDG_RUNTIME_DIR` plus the uid-derived
/// per-user runtime dir (`/run/user/<uid>`, see [`uid_runtime_dir`]), return
/// the ORDERED, de-duplicated list of directories that may hold a watcher's
/// liveness files, always ending with the `/var/run/claude` fallback.
///
/// The uid-derived dir is the ENV-INDEPENDENT spelling of the per-user
/// runtime dir. It exists because the READER'S environment must not decide
/// what it can see: `claude-watch metrics` runs from cron, which does not set
/// `$XDG_RUNTIME_DIR`, while the monitor-mode watchers it is counting write
/// their `<name>.lock` to `/run/user/<uid>` (THEIR `$XDG_RUNTIME_DIR`). With
/// only the env value, the cron reader scanned `/var/run/claude` alone and
/// reported 1 live watcher of 4 while `claude-watch status` (interactive,
/// env set) and the daemon (unit sets the var) both said 4/4.
///
/// Kept pure (params, not `std::env`) so it is hermetically testable.
pub(crate) fn pid_dir_candidates(
    claude_watch_pid_dir: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    uid_runtime_dir: Option<&str>,
) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    let mut push = |d: &str| {
        let d = d.trim();
        if !d.is_empty() && !dirs.iter().any(|e| e == d) {
            dirs.push(d.to_string());
        }
    };
    if let Some(p) = claude_watch_pid_dir {
        push(p);
    }
    if let Some(p) = xdg_runtime_dir {
        push(p);
    }
    if let Some(p) = uid_runtime_dir {
        push(p);
    }
    push("/var/run/claude");
    dirs
}

/// The per-user runtime directory derived from the REAL uid
/// (`/run/user/<uid>`), independent of whether the caller's environment
/// carries `$XDG_RUNTIME_DIR`. Linux-only convention (systemd-logind); on
/// other platforms the dir simply does not exist and scanning it is a no-op.
pub(crate) fn uid_runtime_dir() -> String {
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    format!("/run/user/{}", uid)
}

/// Every candidate directory that may hold a watcher's liveness files.
///
/// Watchers split across TWO write conventions that can land in DIFFERENT
/// directories:
///   * bash flock-guard watchers (`claude-event-watch`, `botchat-wait`) write
///     `<name>.lock` to `$XDG_RUNTIME_DIR` (else `/var/run/claude`);
///   * `watcher::watcher_run`-spawned pollers (`signal-wait-*`) write
///     `<name>.pid` to `$CLAUDE_WATCH_PID_DIR` (else `/var/run/claude`) —
///     `watcher::pid_dir()` IGNORES `$XDG_RUNTIME_DIR`.
///
/// A single env-resolved dir ([`watcher_pid_dir`]) therefore misses whichever
/// convention landed elsewhere, and the miss is asymmetric between a process
/// that HAS `$XDG_RUNTIME_DIR` (interactive `watcher-ctl status` → reads
/// `/run/user/<uid>`, misses the `signal-wait` `.pid` in `/var/run/claude`) and
/// one that does NOT (the daemon started without it → reads `/var/run/claude`,
/// misses the `botchat-wait`/`claude-event-watch` `.lock` in
/// `/run/user/<uid>`). Scanning ALL candidates makes liveness detection
/// independent of which dir a given watcher wrote to and of the reader's own
/// environment. The per-user runtime dir is therefore ALSO derived from the
/// uid (`/run/user/<uid>`, [`uid_runtime_dir`]) rather than trusted to
/// `$XDG_RUNTIME_DIR` alone: the cron-run `claude-watch metrics` has no such
/// var and previously saw only `/var/run/claude`, under-counting
/// `claude_code_live_watchers` (1 of 4 live) while every env-carrying reader
/// agreed on 4/4.
pub(crate) fn watcher_pid_dirs() -> Vec<String> {
    pid_dir_candidates(
        std::env::var("CLAUDE_WATCH_PID_DIR").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        Some(&uid_runtime_dir()),
    )
}

/// Read a watcher PID file (`<name>.pid`) and return the recorded PID, if the
/// file exists and contains a parseable integer. Whitespace is trimmed.
/// `None` on missing / unreadable / non-numeric content.
// Superseded in production by `collect_watcher_recorded_pids` (which scans
// every candidate dir × {lock,pid}); retained as a tested single-dir/single-
// file primitive. dead_code allow: only test callers remain, invisible to the
// lib-only dead-code pass.
#[allow(dead_code)]
pub(crate) fn read_watcher_pid(pid_dir: &str, name: &str) -> Option<u32> {
    let path = format!("{}/{}.pid", pid_dir, name);
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Read the PID the watcher recorded for itself, from the runtime dir.
///
/// A watcher records its live PID in one of two files under [`watcher_pid_dir`]:
///   * `<name>.lock` — written by the watcher itself (the flock singleton
///     guard writes `printf '%s\n' "$$" >&9`). This is the authoritative
///     source in the container, where watchers are spawned by the session as
///     `run_in_background` tasks (NOT via `watcher_run`), so no `.pid` file
///     exists.
///   * `<name>.pid` — written by `watcher::watcher_run` with the child PID when
///     claude-watch spawns the watcher.
///
/// We prefer `<name>.lock` (always present for a live watcher in the container)
/// and fall back to `<name>.pid`. Returns the first file that parses to a PID,
/// or `None` if neither exists / parses.
///
/// PITFALL that motivated [`collect_watcher_recorded_pids`]: the `.lock`
/// preference is WRONG when the `.lock` is STALE (names a dead pid, left behind
/// after a flock-guard watcher's live lock moved to another dir) while the
/// `.pid` beside it is FRESH (names the live poller) — this returned the dead
/// pid and produced a false-DOWN. Production now collects EVERY recorded pid
/// and picks the alive one; this single-pick primitive is retained for its
/// tests.
// dead_code allow: only test callers remain, invisible to the lib-only pass.
#[allow(dead_code)]
pub(crate) fn read_watcher_recorded_pid(pid_dir: &str, name: &str) -> Option<u32> {
    let lock = format!("{}/{}.lock", pid_dir, name);
    if let Ok(content) = std::fs::read_to_string(&lock) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            return Some(pid);
        }
    }
    read_watcher_pid(pid_dir, name)
}

/// Does the live process `pid`'s cmdline look like *this* watcher (identity
/// check to reject a recycled PID the kernel handed to an unrelated process)?
///
/// The match is lenient because the watcher's launcher `exec`s a child or
/// re-execs itself, so the live argv rarely equals the literal `start_cmd`.
/// Concretely, the start_cmd is the launcher SCRIPT
/// (`/opt/claude-container/watchers/claude-event-watch.sh`) but the live
/// process — after `exec /usr/local/bin/claude-event-watch` — has cmdline
/// `/bin/bash /usr/local/bin/claude-event-watch`. The `.sh` is gone, so a
/// naive `cmdline.contains(start_cmd)` fails. We therefore reduce the
/// start_cmd's first token to its basename AND strip a trailing script
/// extension (`.sh`, `.bash`, `.py`), yielding the stem `claude-event-watch`,
/// which DOES appear in the exec'd cmdline. This tolerates the exec-to-binary
/// transform while still rejecting an obviously-unrelated recycled PID (whose
/// cmdline won't contain the watcher's name stem).
pub(crate) fn cmdline_matches_watcher(cmdline: &str, start_cmd: &str) -> bool {
    let token = match start_cmd.split_whitespace().next() {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let base = token.rsplit('/').next().unwrap_or(token);
    // Strip a trailing script extension so a `.sh` launcher that exec's a bare
    // binary of the same stem still matches.
    let stem = strip_script_suffix(base);
    if stem.is_empty() {
        return false;
    }
    cmdline.contains(token) || cmdline.contains(base) || cmdline.contains(stem)
}

/// Strip a trailing watcher-launcher script extension (`.sh`, `.bash`, `.py`)
/// from a file basename, yielding the bare stem. Used so a `.sh` launcher that
/// `exec`s a same-stem binary still matches by identity. Returns the input
/// unchanged when no known extension is present.
pub(crate) fn strip_script_suffix(base: &str) -> &str {
    base.strip_suffix(".sh")
        .or_else(|| base.strip_suffix(".bash"))
        .or_else(|| base.strip_suffix(".py"))
        .unwrap_or(base)
}

/// Pure decision: is the watcher DOWN, given what was observed about its
/// recorded PID file?
///
/// Kept pure (no `/proc`, no `pgrep`, no filesystem) so the DOWN logic is
/// unit-testable.
///
/// Inputs (all already probed by the caller):
/// - `recorded_pid`: the PID read from the watcher's `<name>.lock` / `<name>.pid`
///   file, or `None` if no pidfile exists.
/// - `pid_alive`: whether that recorded PID is currently alive (genuine-liveness
///   probe). Meaningless when `recorded_pid` is `None`.
/// - `cmdline_matches`: whether that PID's `/proc/<pid>/cmdline` matches this
///   watcher's identity (rejects a recycled PID). Meaningless when
///   `recorded_pid` is `None` or `!pid_alive`.
///
/// A watcher is UP iff its pidfile names a live process whose cmdline matches
/// the watcher. DOWN in every other case:
///   * missing pidfile  → DOWN (no recorded instance),
///   * stale pidfile (recorded PID dead) → DOWN (triggers a legit restart),
///   * recycled PID (alive but cmdline mismatch) → DOWN.
///
/// NOTE: there is intentionally no `pgrep` / process-scan path here — `exec`
/// replacing the launcher's argv with the exec'd binary's argv defeats any
/// `pgrep -f <launcher.sh>` match (this bug). Liveness comes ONLY from the
/// pidfile the watcher itself maintains.
pub(crate) fn pidfile_watcher_is_down(
    recorded_pid: Option<u32>,
    pid_alive: bool,
    cmdline_matches: bool,
) -> bool {
    match recorded_pid {
        Some(_) => !(pid_alive && cmdline_matches),
        None => true,
    }
}

/// Convenience: resolve the recorded PID for `name` and decide UP/DOWN using
/// the SAME pidfile-liveness model the daemon's watcher_monitor uses. Performs
/// the `/proc` reads (PID liveness + cmdline identity) and returns
/// `(recorded_pid, is_down)`.
///
/// `start_cmd` is the watcher's configured launch command (used for the
/// cmdline identity check). When `None`, a live recorded PID is accepted
/// without an identity check (we have nothing to reject it with, and the
/// pidfile naming it is itself evidence) — mirroring the daemon's behaviour.
///
/// Superseded in production by [`watcher_pidfile_liveness_multi`], which scans
/// all candidate dirs and both files (fixing the split-dir + stale-`.lock`
/// false-DOWN). Retained as the tested single-dir primitive.
// dead_code allow: only test callers remain, invisible to the lib-only pass.
#[allow(dead_code)]
pub(crate) fn watcher_pidfile_liveness(
    pid_dir: &str,
    name: &str,
    start_cmd: Option<&str>,
) -> (Option<u32>, bool) {
    let recorded_pid = read_watcher_recorded_pid(pid_dir, name);
    let pid_alive = recorded_pid.is_some_and(is_pid_genuinely_alive);
    let cmdline_matches = match (recorded_pid, pid_alive, start_cmd) {
        (Some(pid), true, Some(sc)) => match read_proc_cmdline(pid) {
            Some(cmdline) => cmdline_matches_watcher(&cmdline, sc),
            None => false,
        },
        (Some(_), true, None) => true,
        _ => false,
    };
    let down = pidfile_watcher_is_down(recorded_pid, pid_alive, cmdline_matches);
    (recorded_pid, down)
}

/// Every distinct PID recorded for `name` across ALL candidate pid dirs and
/// BOTH liveness-file conventions (`<name>.lock` written by the bash flock
/// guard, `<name>.pid` written by `watcher_run`). Order: for each dir in
/// `dirs`, the `.lock` pid then the `.pid` pid; duplicates removed preserving
/// first-seen order.
///
/// This replaces the single-file "prefer `.lock` over `.pid`" pick used by
/// [`read_watcher_recorded_pid`]. That single pick mis-selected a STALE
/// `.lock` (naming a dead pid, left behind when a flock-guard watcher's live
/// lock moved to a different dir) over a FRESH `.pid` (naming the live poller)
/// SITTING IN THE SAME DIRECTORY, producing a false-DOWN even though the live
/// pid was recorded right next to the stale one. Collecting every recorded pid
/// and letting the caller pick the one that is genuinely ALIVE fixes that.
pub(crate) fn collect_watcher_recorded_pids(dirs: &[String], name: &str) -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();
    for dir in dirs {
        for suffix in ["lock", "pid"] {
            let path = format!("{}/{}.{}", dir, name, suffix);
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    pids
}

/// Multi-directory / multi-file liveness: the watcher is UP iff ANY pid
/// recorded for it (across all candidate dirs AND both `.lock`/`.pid` files) is
/// genuinely alive and cmdline-matches the watcher. Returns `(pid, down)`:
/// `pid` is the live matching pid when UP, otherwise the FIRST recorded pid
/// seen (for stale→restart diagnostics / `orphaned`), else `None`.
///
/// This is the env-independent replacement for a single
/// [`watcher_pidfile_liveness`] call keyed on one [`watcher_pid_dir`]. It fixes
/// both halves of the split-brain false-DOWN:
///   * a `.pid` in `/var/run/claude` is still found by a reader whose
///     `$XDG_RUNTIME_DIR` resolved [`watcher_pid_dir`] to `/run/user/<uid>`
///     (the interactive `signal-wait` DOWN case), and
///   * a FRESH `.pid` naming a live poller wins over a STALE `.lock` naming a
///     dead pid in the SAME dir (the daemon `botchat-wait`/`claude-event-watch`
///     DOWN case), because liveness is decided over every recorded pid, not a
///     single lock-preferring pick.
///
/// UP still requires a GENUINELY-alive, cmdline-matching process, so a watcher
/// that is truly dead (all recorded pids dead / mismatched) still reports DOWN
/// and triggers a legitimate restart.
pub(crate) fn watcher_pidfile_liveness_multi(
    dirs: &[String],
    name: &str,
    start_cmd: Option<&str>,
) -> (Option<u32>, bool) {
    let pids = collect_watcher_recorded_pids(dirs, name);
    let first = pids.first().copied();
    for pid in &pids {
        let pid_alive = is_pid_genuinely_alive(*pid);
        let cmdline_matches = match (pid_alive, start_cmd) {
            (true, Some(sc)) => read_proc_cmdline(*pid)
                .map(|cmdline| cmdline_matches_watcher(&cmdline, sc))
                .unwrap_or(false),
            // No start_cmd to compare against → a live recorded pid is itself
            // evidence (mirrors the single-dir `watcher_pidfile_liveness`).
            (true, None) => true,
            (false, _) => false,
        };
        // Reuse the pure UP/DOWN decision so this multi path and the daemon's
        // single-instance model stay in lockstep.
        if !pidfile_watcher_is_down(Some(*pid), pid_alive, cmdline_matches) {
            return (Some(*pid), false);
        }
    }
    (first, true)
}

/// Youngest runtime-file age (seconds) for `name` across ALL candidate dirs —
/// the multi-directory analogue of [`watcher_runtime_file_age_secs`]. Used by
/// the daemon's grace window so a watcher whose freshest `.lock`/`.pid` was
/// just rewritten in ANY candidate dir stays in-grace across the exit→restart
/// gap, regardless of which dir that convention wrote to.
#[allow(dead_code)] // sole non-test caller is the daemon's watcher_monitor (see watcher_runtime_file_age_secs)
pub(crate) fn watcher_runtime_file_age_secs_multi(dirs: &[String], name: &str) -> Option<f64> {
    dirs.iter()
        .filter_map(|d| watcher_runtime_file_age_secs(d, name))
        .fold(None, |acc, age| Some(acc.map_or(age, |cur: f64| cur.min(age))))
}

/// Age (seconds) since the most-recently-modified watcher runtime file
/// (`<name>.lock`, `<name>.pid`, `<name>.runlock`) under `pid_dir`. Returns
/// `None` if none of the three exist.
///
/// A freshly-written pidfile is proof the watcher was (re)spawned recently. The
/// watcher-monitor uses this to keep a fire-and-exit watcher in its grace window
/// across the brief exit->restart gap even when the daemon's poll never caught
/// it genuinely alive: `<name>.lock`/`<name>.pid` are rewritten on EVERY restart
/// (by the watcher's flock guard / `watcher_run`), so as long as the main loop
/// keeps restarting within the grace window, the freshest mtime stays young. A
/// GENUINELY dead watcher (main loop stopped restarting) has its pidfiles age
/// past the window, so real DOWN detection is preserved.
///
/// Pure-ish: filesystem stats only, no `pgrep` / `/proc`.
// dead_code allow: the sole non-test caller is `policy::check_cycle`, which
// the lib-only dead-code pass (release build, RUSTFLAGS=-D warnings) does not
// treat as a root (it's reached via the `bin`'s `main`). Genuinely used by the
// running daemon's watcher_monitor; the allow keeps `-D warnings` green.
#[allow(dead_code)]
pub(crate) fn watcher_runtime_file_age_secs(pid_dir: &str, name: &str) -> Option<f64> {
    let now = SystemTime::now();
    let suffixes = ["lock", "pid", "runlock"];
    let mut youngest: Option<f64> = None;
    for suffix in suffixes {
        let path = format!("{}/{}.{}", pid_dir, name, suffix);
        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Age = now - mtime. A clock skew that puts mtime in the future
        // clamps to 0 (treat a future-dated file as freshly written, never
        // negative) so a skewed clock can't manufacture a stale reading.
        let age = now
            .duration_since(mtime)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        youngest = Some(match youngest {
            Some(cur) => cur.min(age),
            None => age,
        });
    }
    youngest
}

/// Pure decision: is a watcher still within its grace window?
///
/// The grace window is anchored on RECENT PROOF-OF-LIFE that does NOT require
/// the daemon to catch the short-lived (fire-and-exit) watcher mid-flight. The
/// watcher is in-grace if EITHER:
///   * `last_seen_age` (seconds since the last poll that caught it genuinely
///     running) is within `grace_secs`, OR
///   * `pidfile_age` (seconds since its freshest runtime file was written, from
///     [`watcher_runtime_file_age_secs`]) is within `grace_secs`.
///
/// The pidfile anchor is what kills the restart-gap false-DOWN: a watcher whose
/// pidfile was just rewritten is mid-restart-cycle, NOT down, even when the
/// daemon's poll never observed it alive between exit and respawn.
#[allow(dead_code)] // see watcher_runtime_file_age_secs: lib-only dead-code false positive
pub(crate) fn watcher_in_grace(
    last_seen_age: Option<f64>,
    pidfile_age: Option<f64>,
    grace_secs: f64,
) -> bool {
    last_seen_age.is_some_and(|e| e < grace_secs)
        || pidfile_age.is_some_and(|e| e < grace_secs)
}

/// Age (seconds) since the freshest clean-exit marker (`<name>.exit`) for
/// `name` across ALL candidate dirs, or `None` if no marker exists.
///
/// A block-print-exit watcher writes this marker (via `date > <name>.exit`)
/// immediately before its DELIBERATE `exit 0`, i.e. after it has blocked,
/// collected a batch, printed it, and emitted the restart banner. The marker is
/// therefore proof that a watcher instance exited CLEANLY (delivered its
/// payload), as opposed to crashing or never starting. The daemon's
/// watcher-monitor pairs this age with the pidfile age (see
/// [`watcher_cleanly_exited_recently`]) to keep a cleanly-exited watcher in
/// grace across the delivery->restart gap without masking a real crash.
///
/// Pure-ish: filesystem stats only, no `pgrep` / `/proc`.
#[allow(dead_code)] // sole non-test caller is the daemon's watcher_monitor (lib-only dead-code false positive)
pub(crate) fn watcher_clean_exit_age_secs_multi(dirs: &[String], name: &str) -> Option<f64> {
    let now = SystemTime::now();
    dirs.iter()
        .filter_map(|d| {
            let path = format!("{}/{}.exit", d, name);
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            // Clamp a future-dated mtime (clock skew) to 0 so skew can't
            // manufacture a stale reading (mirrors watcher_runtime_file_age).
            Some(
                now.duration_since(mtime)
                    .map(|x| x.as_secs_f64())
                    .unwrap_or(0.0),
            )
        })
        .fold(None, |acc, age| Some(acc.map_or(age, |cur: f64| cur.min(age))))
}

/// Pure decision: is the watcher in the benign "cleanly exited, restart
/// pending" state?
///
/// Returns `true` iff the clean-exit marker (`clean_exit_age`) is:
///   1. FRESHER than the watcher's pidfile (`clean_exit_age < pidfile_age`) —
///      so it was written by the CURRENTLY-recorded instance (the pidfile is
///      rewritten on every restart, so a marker predating it belongs to a
///      previous instance and the current one did NOT exit cleanly), AND
///   2. younger than `clean_exit_grace_secs`.
///
/// Why the pidfile comparison is load-bearing (crash preservation): if a
/// watcher is restarted and then CRASHES without a clean exit, its pidfile is
/// fresh (rewritten at restart) but the newest `.exit` marker is from the
/// PREVIOUS clean exit — older than the pidfile. `clean_exit_age < pidfile_age`
/// is then false, so a crash-after-restart still reports DOWN promptly rather
/// than being masked for the whole grace window.
///
/// A missing pidfile (`pidfile_age == None`) means the watcher never recorded a
/// live PID — treated as not-cleanly-exited (return `false`) so a stray marker
/// alone can never suppress DOWN.
#[allow(dead_code)] // sole non-test caller is the daemon's watcher_monitor (lib-only dead-code false positive)
pub(crate) fn watcher_cleanly_exited_recently(
    clean_exit_age: Option<f64>,
    pidfile_age: Option<f64>,
    clean_exit_grace_secs: f64,
) -> bool {
    match (clean_exit_age, pidfile_age) {
        (Some(ce), Some(pf)) => ce < pf && ce < clean_exit_grace_secs,
        _ => false,
    }
}

/// Age (seconds) of the freshest `<name>.monitor-intent` for `name` across
/// ALL candidate dirs, or `None` if no intent file exists.
///
/// `watcher-ctl run <name>` writes this file for a `mode=monitor` watcher
/// instead of exec'ing it (`epoch=<secs>\ncommand=<monitor_cmd>\n`) and
/// prints the Monitor-tool command for the main loop to arm. The `epoch=`
/// line is the authoritative timestamp (it is what the writer meant); the
/// file mtime is the fallback for a hand-written or truncated file. A
/// future-dated value (clock skew) clamps to 0 so skew can never
/// manufacture a stale reading (mirrors `watcher_runtime_file_age_secs`).
///
/// Pure-ish: filesystem reads only, no `pgrep` / `/proc`.
pub(crate) fn watcher_monitor_intent_age_secs_multi(dirs: &[String], name: &str) -> Option<f64> {
    let now = SystemTime::now();
    let now_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    dirs.iter()
        .filter_map(|d| {
            let path = format!("{}/{}.monitor-intent", d, name);
            let meta = std::fs::metadata(&path).ok()?;
            let from_epoch = std::fs::read_to_string(&path).ok().and_then(|body| {
                body.lines()
                    .find_map(|l| l.strip_prefix("epoch="))
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .map(|e| (now_epoch - e).max(0.0))
            });
            from_epoch.or_else(|| {
                meta.modified().ok().map(|mtime| {
                    now.duration_since(mtime)
                        .map(|x| x.as_secs_f64())
                        .unwrap_or(0.0)
                })
            })
        })
        .fold(None, |acc, age| Some(acc.map_or(age, |cur: f64| cur.min(age))))
}

/// Pure decision: is a `mode=monitor` watcher that currently has NO live pid
/// in its ARMING window (healthy-pending, not DOWN)?
///
/// ARMING iff an arm intent exists (`intent_age`), it is younger than
/// `arming_grace_secs`, AND no runtime file (`.lock`/`.pid`/`.runlock`,
/// `pidfile_age`) has been written SINCE the intent. The last clause is what
/// keeps a real outage visible: the monitor's flock guard rewrites
/// `<name>.lock` when it goes live, so a runtime file YOUNGER than the intent
/// proves the arm was consumed — if the watcher is down after that, it DIED
/// and must read DOWN at once, not ride out the rest of the window. A runtime
/// file OLDER than the intent (left over from the one-shot era, or a stale
/// lock `watcher-restart` did not clean) does not consume it.
///
/// `arming_grace_secs <= 0` disables the state entirely (an un-armed monitor
/// reads DOWN immediately, the pre-ARMING behaviour).
pub(crate) fn watcher_is_arming(
    intent_age: Option<f64>,
    pidfile_age: Option<f64>,
    arming_grace_secs: f64,
) -> bool {
    if arming_grace_secs <= 0.0 {
        return false;
    }
    match intent_age {
        Some(ia) if ia < arming_grace_secs => pidfile_age.is_none_or(|pf| pf > ia),
        _ => false,
    }
}

/// Resolve the monitor-mode ARMING grace for a ONE-SHOT CLI call
/// (`watcher-ctl status` / `watcher-status --unhealthy-only`, which do not
/// carry the daemon's `Config`). Order: `$CLAUDE_WATCH_MONITOR_ARMING_GRACE_SECS`
/// (non-empty, parseable) → `[watcher_monitor].monitor_arming_grace_secs`
/// from the layered config if one loads → the code default. The daemon
/// passes its own config value directly and never calls this.
pub(crate) fn resolve_monitor_arming_grace_secs() -> f64 {
    if let Ok(v) = std::env::var("CLAUDE_WATCH_MONITOR_ARMING_GRACE_SECS") {
        if let Ok(n) = v.trim().parse::<u64>() {
            return n as f64;
        }
    }
    crate::config::try_load_config()
        .map(|c| c.watcher_monitor.monitor_arming_grace_secs)
        .unwrap_or(crate::config::DEFAULT_MONITOR_ARMING_GRACE_SECS) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TmuxConfig;

    // -------------------------------------------------------------------
    // prefer_configured_pane — the config-branch decision behind the
    // focus-follows-inject fix. When a dashboard_session is configured the
    // daemon must resolve the FIXED main-loop pane (via find_dashboard_pane)
    // instead of the unconstrained `find_claude_pane()` scan, so a
    // MAIN-LOOP-SCOPED inject can never land in an operator-focused TUI
    // agent-view subagent pane. These pin the branch logic without a live
    // tmux (the actual pane lookup is covered by the live-tmux e2e test).
    // -------------------------------------------------------------------

    #[test]
    fn prefer_configured_pane_true_when_session_set() {
        // The in-container config: dashboard_session = "claude-container",
        // dashboard_pane = "claude-container:0.0". Configured => prefer the
        // fixed pane, never the active/first-scanned pane.
        let cfg = TmuxConfig {
            dashboard_pane: "claude-container:0.0".to_string(),
            dashboard_session: "claude-container".to_string(),
            post_escape_settle_ms: 0,
            ..Default::default()
        };
        assert!(
            prefer_configured_pane(&cfg),
            "a configured dashboard_session MUST prefer the fixed main-loop pane"
        );
    }

    #[test]
    fn prefer_configured_pane_true_with_only_session() {
        // Session set but pane left empty: still prefer config — find_dashboard_pane
        // resolves a shell pane within the session, which is more targeted than
        // the global `claude`-command scan.
        let cfg = TmuxConfig {
            dashboard_pane: String::new(),
            dashboard_session: "claude-container".to_string(),
            post_escape_settle_ms: 0,
            ..Default::default()
        };
        assert!(prefer_configured_pane(&cfg));
    }

    #[test]
    fn prefer_configured_pane_false_when_unconfigured() {
        // Fresh install / host dev: nothing configured => fall back to the
        // historical auto-detect scan (single-claude layouts are unambiguous).
        let cfg = TmuxConfig::default();
        assert!(
            !prefer_configured_pane(&cfg),
            "an unconfigured tmux section MUST fall back to the auto-detect scan"
        );
    }

    #[test]
    fn prefer_configured_pane_false_when_only_pane_set_without_session() {
        // Defensive: dashboard_pane without a session can't be session-verified
        // by find_dashboard_pane (it early-returns find_claude_pane when session
        // is empty), so the decision is driven by the session field.
        let cfg = TmuxConfig {
            dashboard_pane: "claude-container:0.0".to_string(),
            dashboard_session: String::new(),
            post_escape_settle_ms: 0,
            ..Default::default()
        };
        assert!(!prefer_configured_pane(&cfg));
    }

    #[test]
    fn active_ui_true_for_agent_roster_row() {
        let pane = "\u{25ef} general-purpose    Scanning claude-w\u{2026} 3m 14s \u{b7} \u{2193} 102.8k tokens\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_true_for_thinking_indicator() {
        let pane = "\u{25cf} Zigzagging\u{2026} (37s \u{b7} \u{2193} 1.3k tokens \u{b7} thought for 13s)\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_true_for_background_tasks_overlay() {
        let pane = "Background tasks\n  Shells (2)\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_false_for_fresh_idle_pane() {
        // A genuinely fresh/idle session: permission-mode status bar + empty
        // prompt, no thinking indicator, no roster, no overlay.
        let pane = "\u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt\n\u{276f} ";
        assert!(!pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_false_for_bare_status_bar_total() {
        // Status bar showing the bare context total but no active-work markers.
        let pane = "224598 tokens\n\u{23f5}\u{23f5} bypass permissions on\n\u{276f} ";
        assert!(!pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_true_for_monitors_still_running_completion_tail() {
        // 2026-08-27 regression, confirmed from Andrew's screenshot: the
        // "fresh session" resume prompt fired while the pane showed exactly
        // this completion-tail line -- 47 minutes into an active session with
        // two live Monitor-tool watches, not a fresh/idle pane.
        let pane = "\u{273b} Brewed for 47m 32s \u{00b7} 2 monitors still running\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_true_for_background_tasks_still_running_completion_tail() {
        let pane = "\u{273b} Cogitated for 2m 11s \u{00b7} 6 background tasks still running\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_true_for_truncated_still_running_completion_tail() {
        // Narrow-pane truncation renders "still…" instead of "still running".
        let pane = "\u{273b} Cogitated for 2m 11s \u{00b7} 6 tasks still\u{2026}\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn active_ui_true_for_bare_monitors_status_bar_counter() {
        let pane = "\u{23f5}\u{23f5} bypass permissions on \u{00b7} 2 monitors \u{00b7} \u{2190} for agents \u{00b7} \u{2193} to manage\n\u{276f} ";
        assert!(pane_shows_active_ui(pane));
    }

    #[test]
    fn test_watcher_runtime_file_age_secs_none_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            watcher_runtime_file_age_secs(dir.path().to_str().unwrap(), "evw"),
            None
        );
    }

    #[test]
    fn test_watcher_runtime_file_age_secs_fresh_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("evw.lock"), "70071").unwrap();
        let age = watcher_runtime_file_age_secs(dir.path().to_str().unwrap(), "evw")
            .expect("a just-written lock file must yield Some(age)");
        // Just-written file: age is essentially zero, certainly well under a
        // generous bound.
        assert!(age >= 0.0, "age must be non-negative, got {age}");
        assert!(age < 5.0, "freshly-written lock should be young, got {age}");
    }

    #[test]
    fn test_watcher_runtime_file_age_secs_takes_freshest() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        // Write a .pid first, then make the .lock the fresher of the two by
        // backdating the .pid's mtime well past the .lock's.
        std::fs::write(dir.path().join("evw.pid"), "111").unwrap();
        std::fs::write(dir.path().join("evw.lock"), "222").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        filetime_set(&dir.path().join("evw.pid"), old);
        let age = watcher_runtime_file_age_secs(d, "evw").unwrap();
        // The freshest (.lock) is young; the result must reflect IT, not the
        // hour-old .pid.
        assert!(age < 5.0, "should report freshest (lock) age, got {age}");
    }

    // Helper: backdate a file's mtime (std-only; `File::set_modified`, 1.75+).
    fn filetime_set(path: &std::path::Path, t: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn test_watcher_in_grace_pidfile_fresh_overrides_stale_last_seen() {
        // The regression case: last_seen_running is STALE (well past grace) but
        // the pidfile was just rewritten (mid restart-cycle). The watcher must
        // be treated as IN-GRACE (not a miss / not DOWN).
        let grace = 90.0;
        assert!(
            watcher_in_grace(Some(300.0), Some(2.0), grace),
            "fresh pidfile must keep a stale-last-seen watcher in grace"
        );
    }

    #[test]
    fn test_watcher_in_grace_both_stale_is_not_in_grace() {
        // Genuinely down: last_seen stale AND pidfile aged past grace -> NOT in
        // grace, so real DOWN detection still fires.
        let grace = 90.0;
        assert!(!watcher_in_grace(Some(300.0), Some(300.0), grace));
        assert!(!watcher_in_grace(Some(300.0), None, grace));
        assert!(!watcher_in_grace(None, None, grace));
    }

    #[test]
    fn test_watcher_in_grace_either_anchor_fresh() {
        let grace = 90.0;
        // Fresh last_seen alone keeps it in grace (legacy behaviour preserved).
        assert!(watcher_in_grace(Some(10.0), None, grace));
        // Fresh pidfile alone keeps it in grace (the new anchor).
        assert!(watcher_in_grace(None, Some(10.0), grace));
    }

    // --- monitor-mode ARMING grace tests ---

    #[test]
    fn arming_true_for_fresh_unconsumed_intent() {
        // Intent written 5s ago, no runtime file at all: the main loop just
        // ran `watcher-ctl run` and has not armed the Monitor yet.
        assert!(watcher_is_arming(Some(5.0), None, 120.0));
        // A runtime file OLDER than the intent (stale one-shot lock) does not
        // consume the intent.
        assert!(watcher_is_arming(Some(5.0), Some(3600.0), 120.0));
    }

    #[test]
    fn arming_false_when_intent_stale_or_missing() {
        // Past the grace window with nothing live -> DOWN again.
        assert!(!watcher_is_arming(Some(121.0), None, 120.0));
        assert!(!watcher_is_arming(Some(120.0), None, 120.0));
        // No intent was ever recorded -> plain DOWN.
        assert!(!watcher_is_arming(None, None, 120.0));
        assert!(!watcher_is_arming(None, Some(1.0), 120.0));
    }

    #[test]
    fn arming_false_when_runtime_file_is_younger_than_intent() {
        // The monitor went live AFTER the intent (its flock guard rewrote
        // <name>.lock) and is now dead: that is a real outage, not an arm in
        // progress — must NOT ride out the rest of the window as ARMING.
        assert!(!watcher_is_arming(Some(60.0), Some(10.0), 120.0));
        // Equal ages are treated as consumed too (not strictly older).
        assert!(!watcher_is_arming(Some(10.0), Some(10.0), 120.0));
    }

    #[test]
    fn arming_disabled_when_grace_is_zero() {
        assert!(!watcher_is_arming(Some(1.0), None, 0.0));
        assert!(!watcher_is_arming(Some(0.0), None, 0.0));
    }

    #[test]
    fn monitor_intent_age_prefers_epoch_line_and_takes_freshest_across_dirs() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Dir 1: intent stamped 500s ago (epoch line wins over the fresh mtime).
        std::fs::write(
            d1.path().join("evw.monitor-intent"),
            format!("epoch={}\ncommand=evw --mode monitor\n", now - 500),
        )
        .unwrap();
        // Dir 2: intent stamped 30s ago.
        std::fs::write(
            d2.path().join("evw.monitor-intent"),
            format!("epoch={}\ncommand=evw --mode monitor\n", now - 30),
        )
        .unwrap();
        let dirs = vec![
            d1.path().to_str().unwrap().to_string(),
            d2.path().to_str().unwrap().to_string(),
        ];
        let age = watcher_monitor_intent_age_secs_multi(&dirs, "evw").expect("intent present");
        assert!((29.0..35.0).contains(&age), "freshest intent wins: {}", age);
        // Single stale dir alone reads ~500.
        let age1 = watcher_monitor_intent_age_secs_multi(&dirs[..1], "evw").unwrap();
        assert!((499.0..505.0).contains(&age1), "{}", age1);
        // Unknown watcher / no file -> None.
        assert!(watcher_monitor_intent_age_secs_multi(&dirs, "nope").is_none());
    }

    #[test]
    fn monitor_intent_age_falls_back_to_mtime_without_epoch_line() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("evw.monitor-intent"), "command=evw --mode monitor\n")
            .unwrap();
        let dirs = vec![d.path().to_str().unwrap().to_string()];
        let age = watcher_monitor_intent_age_secs_multi(&dirs, "evw").expect("intent present");
        assert!(age < 5.0, "just-written file reads fresh via mtime: {}", age);
        // A future-dated epoch clamps to 0, never negative.
        let far = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 10_000;
        std::fs::write(d.path().join("evw.monitor-intent"), format!("epoch={}\n", far)).unwrap();
        assert_eq!(watcher_monitor_intent_age_secs_multi(&dirs, "evw"), Some(0.0));
    }

    // --- clean-exit grace (block-print-exit flap fix) tests ---

    #[test]
    fn clean_exit_recent_true_when_marker_fresher_than_pidfile_and_young() {
        // The benign case: the watcher delivered + exited 0 AFTER its last
        // restart (marker fresher than pidfile) and recently (within window).
        // clean_exit_age = 20s, pidfile_age = 90s (restart 90s ago, exited 20s
        // ago), window = 600 -> in clean-exit grace.
        assert!(watcher_cleanly_exited_recently(Some(20.0), Some(90.0), 600.0));
    }

    #[test]
    fn clean_exit_recent_false_when_marker_older_than_pidfile_crash_case() {
        // Crash-after-restart: pidfile fresh (restart 10s ago) but the newest
        // marker is from a PREVIOUS clean exit (120s ago) -> marker OLDER than
        // pidfile -> NOT clean-exited, so DOWN still fires promptly.
        assert!(!watcher_cleanly_exited_recently(Some(120.0), Some(10.0), 600.0));
    }

    #[test]
    fn clean_exit_recent_false_when_marker_past_window_dead_session_case() {
        // Dead session: watcher exited cleanly, never restarted. The marker is
        // fresher than the (also stale) pidfile but has aged past the window ->
        // NOT graced, so a sustained down is still surfaced.
        assert!(!watcher_cleanly_exited_recently(Some(700.0), Some(900.0), 600.0));
    }

    #[test]
    fn clean_exit_recent_false_when_no_pidfile_or_no_marker() {
        // No pidfile anchor -> never suppress (a stray marker alone can't gate).
        assert!(!watcher_cleanly_exited_recently(Some(5.0), None, 600.0));
        // No marker at all -> not clean-exited.
        assert!(!watcher_cleanly_exited_recently(None, Some(5.0), 600.0));
        assert!(!watcher_cleanly_exited_recently(None, None, 600.0));
    }

    #[test]
    fn clean_exit_age_multi_reads_marker_and_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_str().unwrap().to_string()];
        // No marker yet.
        assert_eq!(watcher_clean_exit_age_secs_multi(&dirs, "cew"), None);
        // Fresh marker -> young age.
        std::fs::write(dir.path().join("cew.exit"), "1786518000").unwrap();
        let age = watcher_clean_exit_age_secs_multi(&dirs, "cew")
            .expect("a just-written .exit marker must yield Some(age)");
        assert!(age >= 0.0 && age < 5.0, "fresh marker should be young, got {age}");
    }

    #[test]
    fn clean_exit_age_multi_takes_youngest_across_dirs() {
        let old_dir = tempfile::tempdir().unwrap();
        let fresh_dir = tempfile::tempdir().unwrap();
        std::fs::write(old_dir.path().join("cew.exit"), "1").unwrap();
        filetime_set(
            &old_dir.path().join("cew.exit"),
            SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        std::fs::write(fresh_dir.path().join("cew.exit"), "2").unwrap();
        let dirs = vec![
            old_dir.path().to_str().unwrap().to_string(),
            fresh_dir.path().to_str().unwrap().to_string(),
        ];
        let age = watcher_clean_exit_age_secs_multi(&dirs, "cew").unwrap();
        assert!(age < 60.0, "must report youngest marker's age, got {age}");
    }

    // --- parse_status_bar tests ---

    #[test]
    fn test_parse_status_bar_full() {
        let input = "some output\nmore output\n\
                      50,000 tokens  10 bashes\n\
                      Context left until auto-compact: 85%";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(50000));
        assert_eq!(parsed.bashes, Some(10));
        assert_eq!(parsed.compact_remaining, Some(85));
    }

    #[test]
    fn test_parse_status_bar_tokens_no_commas() {
        let input = "-- INSERT -- 5000 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(5000));
    }

    /// Real pane capture, 2026-08-20 — the agent-roster rows Claude Code
    /// draws BELOW the status bar carry each subagent's own token count.
    /// A just-spawned agent's count is a plain sub-1000 integer, which the
    /// generic `N tok` match happily consumed; being the LAST match in the
    /// bottom-10 window it overwrote the session total.
    ///
    /// Downstream that reads as the token count collapsing from 169233 to
    /// 119 — indistinguishable from a context clear — and the daemon
    /// injected a post-clear resume prompt into a session that had never
    /// been cleared. Three fires in seven minutes.
    #[test]
    fn test_agent_roster_row_does_not_clobber_session_total_2026_08_20() {
        let input = "\
\u{25cf} Read agent output bbow3km6m\n\
  \u{239d}  Read 7 lines\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  \u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 shells \u{00b7} esc to interrupt\n\
                                                     169233 tokens\n\
\n\
  \u{25cf} main\n\
  \u{25ef} general-purpose    Scanning claude-w\u{2026} 3m 14s \u{00b7} \u{2193} 102.8k tokens\n\
  \u{25ef} grafana-dashboard  Listing panels in\u{2026} 1m 28s \u{00b7} \u{2193} 90.6k tokens\n\
  \u{25ef} general-purpose    Inspecting subtor\u{2026}     7s \u{00b7} \u{2193} 119 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(
            parsed.tokens,
            Some(169233),
            "the status bar's bare total must win over a freshly-spawned \
             agent's roster row (↓ 119 tokens) — reading 119 as the session \
             context size is what manufactured a phantom context clear"
        );
        assert_eq!(parsed.bashes, Some(5));
    }

    /// The same shape, but with EVERY roster count already past 1000 so it
    /// renders with a `k` suffix. This case never broke (the `.` in `1.2k`
    /// defeats the generic match), and must keep working.
    #[test]
    fn test_agent_roster_rows_with_k_suffix_still_yield_session_total() {
        let input = "\
  \u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 shells \u{00b7} esc to interrupt\n\
                                                     224598 tokens\n\
\n\
  \u{25cf} main\n\
  \u{25ef} general-purpose    Scanning claude-w\u{2026} 3m 14s \u{00b7} \u{2193} 102.8k tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(224598));
    }

    /// With no bare context total on the status bar, NEITHER a thinking
    /// indicator (`↓ 26000 tokens`, current-turn output) NOR a subagent's
    /// roster row (`↓ 119 tokens`) is trusted as the session total: both are
    /// refused and `tokens` stays None (2026-08 hardening). The pane is still a
    /// recognized UI state, so it is not a parse miss.
    #[test]
    fn test_thinking_indicator_and_roster_both_refused_in_fallback() {
        let input = "\
\u{2733} Boogieing\u{2026} (4s \u{00b7} \u{2193} 26000 tokens)\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt\n\
  \u{25cf} main\n\
  \u{25ef} general-purpose    Inspecting subtor\u{2026}     7s \u{00b7} \u{2193} 119 tokens";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(
            parsed.tokens, None,
            "neither a thinking indicator nor a roster row may stand in for the \
             session context total — reading either manufactured phantom \
             context clears"
        );
        assert!(saw_bar);
    }

    /// A roster row alone yields NO session total. Its `↓ 119 tokens` is one
    /// subagent's count; adopting it as the session context size is exactly the
    /// misparse that manufactured phantom context clears (it collapses a
    /// six-figure context to 119). `tokens` must be None -- the downstream
    /// carry-forward guard (`policy::carry_forward_token_misparse`) holds the
    /// prior real value rather than trusting this 0, so "no better than 0" no
    /// longer applies.
    #[test]
    fn test_agent_roster_row_alone_yields_no_session_total() {
        let input = "\
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt\n\
  \u{25ef} general-purpose    Inspecting subtor\u{2026}     7s \u{00b7} \u{2193} 119 tokens";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(parsed.tokens, None);
        assert!(saw_bar);
    }

    #[test]
    fn test_is_agent_roster_row_discriminates_from_thinking_indicator() {
        assert!(is_agent_roster_row(
            "  \u{25ef} general-purpose    Scanning\u{2026} 3m 14s \u{00b7} \u{2193} 102.8k tokens"
        ));
        // Thinking indicator: same bullet in some versions, but it always
        // parenthesises its counters.
        assert!(!is_agent_roster_row(
            "\u{25cf} Zigzagging\u{2026} (37s \u{00b7} \u{2193} 1.3k tokens \u{00b7} thought for 13s)"
        ));
        // A roster row with no token count yet is not a token source.
        assert!(!is_agent_roster_row("  \u{25cf} main"));
        // Ordinary tool-call output lines share the bullet.
        assert!(!is_agent_roster_row(
            "\u{25cf} Read agent output bbow3km6m"
        ));
    }

    /// The PRIMARY per-line (bottom-10) pass must refuse both kinds of
    /// arrow-token line just as the whole-pane fallback does: neither a roster
    /// row (`↓ 102.8k tokens`) nor a real thinking indicator (`↓ 26000
    /// tokens`) is the session total, so `tokens` stays None even though both
    /// match `token_thinking_re`.
    #[test]
    fn test_thinking_indicator_and_roster_both_refused_in_primary_pass() {
        let input = "\
  \u{25ef} general-purpose    Scanning claude-w\u{2026} 3m 14s \u{00b7} \u{2193} 102.8k tokens\n\
\u{2733} Boogieing\u{2026} (4s \u{00b7} \u{2193} 26000 tokens)";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(
            parsed.tokens, None,
            "no arrow-prefixed token count in the bottom-10 window may become \
             the session total"
        );
        assert!(saw_bar);
    }

    #[test]
    fn test_parse_status_bar_large_tokens() {
        let input = "bypass permissions on · 1,234,567 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(1234567));
    }

    #[test]
    fn test_parse_status_bar_background_tasks() {
        let input = "3 background tasks";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(3));
    }

    #[test]
    fn test_parse_status_bar_bashes() {
        let input = "5 bashes";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_shells() {
        // Claude Code 2.1.94+ renamed "background tasks" / "bashes" to "shells".
        let input = "7 shells";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(7));
    }

    #[test]
    fn test_parse_status_bar_monitors() {
        // 2026-08-27 regression: Claude Code's status bar renders live
        // Monitor-tool background watches as `· N monitors ·`, exactly like
        // shells/background-tasks/bashes. Before this fix `monitor(s)?` was
        // missing from the alternation, so a pane with 0 bashes but 2 live
        // monitors parsed `bashes == 0` -- the first domino in the
        // "tokens==0 && bashes==0 -> dead process" misfire.
        let input = "\u{23f5}\u{23f5} bypass permissions on \u{00b7} 2 monitors \u{00b7} \u{2190} for agents \u{00b7} \u{2193} to manage";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(2));
    }

    #[test]
    fn test_parse_status_bar_singular_monitor() {
        let input = "1 monitor";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(1));
    }

    #[test]
    fn test_parse_status_bar_shells_realistic() {
        // Full realistic status bar line as emitted by Claude Code 2.1.94+
        // in the dashboard pane.
        let input = "output\n\
                     \u{23f5}\u{23f5} bypass permissions on \u{00b7} 6 shells \u{00b7} esc to interrupt \u{00b7} \u{2193} to manage   849577 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(849577));
        assert_eq!(parsed.bashes, Some(6));
    }

    #[test]
    fn test_parse_status_bar_missing_fields() {
        let input = "nothing relevant here\njust some text";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, None);
        assert_eq!(parsed.bashes, None);
        assert_eq!(parsed.compact_remaining, None);
    }

    #[test]
    fn test_parse_status_bar_empty() {
        let parsed = parse_status_bar("");
        assert_eq!(parsed, ParsedStatusBar::default());
    }

    #[test]
    fn test_parse_status_bar_only_last_10_lines() {
        let mut lines = vec!["99,999 tokens"];
        for _ in 0..15 {
            lines.push("filler line");
        }
        let input = lines.join("\n");
        let parsed = parse_status_bar(&input);
        // Token line is beyond last 10 lines, should not be found
        assert_eq!(parsed.tokens, None);
    }

    #[test]
    fn test_parse_status_bar_compact_zero() {
        let input = "Context left until auto-compact: 0%";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.compact_remaining, Some(0));
    }

    #[test]
    fn test_parse_status_bar_realistic() {
        // Realistic Claude Code status bar content
        let input = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f} \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT --  123,456 tokens  5 bashes  Context left until auto-compact: 42%\n\
                      current: 2.1.77   latest: 2.1.78";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(123456));
        assert_eq!(parsed.bashes, Some(5));
        assert_eq!(parsed.compact_remaining, Some(42));
    }

    #[test]
    fn test_parse_status_bar_wrapped_narrow_pane() {
        // When tmux pane is narrow, the status bar wraps across lines.
        // "bypass permissions" is on one line, "175630 tokens" on the next.
        let input = "some output\n\
                     more output\n\
                     \u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 shells \u{00b7} esc to interrupt \u{00b7} \u{2193}\u{2026}\n\
                     175630 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(175630));
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_shells_wrapped_permissi() {
        // Real capture from Claude Code 2.1.94: status bar uses "N shells"
        // (new terminology) AND the word "permissions" is wrapped, splitting
        // into "bypass permissi ·  on". Previously neither the has_status_bar
        // check nor bash_re matched "shells", so tokens + bashes both parsed
        // as None and the daemon emitted 696 ClaudeProcessDead false alerts
        // in a few hours.
        let input = "some output\n\
                     \u{23f5}\u{23f5} bypass permissi \u{00b7}  on   5 shells \u{00b7} esc to interrupt \u{00b7} \u{2193} to manage   580828 tokens\n\
                     current: 2.1.94 \u{00b7} latest: 2.1.96";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(580828));
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_truncated_ellipsis() {
        // Real capture from a pane where Claude Code truncated the status bar
        // with an ellipsis: "bypass permissi" (not "permissions") and
        // "502064 tok…" (not "tokens"). Previously parsed as tokens=None
        // which caused spurious ClaudeProcessDead Prometheus alerts.
        let input = "output line\n\
                     \u{23f5}\u{23f5} bypass permissi \u{00b7}  on   6 background tasks \u{00b7} ctrl+x ctrl+k to stop agen502064 tok\u{2026}";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(502064));
        assert_eq!(parsed.bashes, Some(6));
    }

    #[test]
    fn test_parse_status_bar_wrapped_with_compact() {
        // Wrapped status bar with compact info on a separate line
        let input = "output\n\
                     \u{23f5}\u{23f5} bypass permissions on \u{00b7} 3 bashes \u{00b7} esc to interrupt\n\
                     42,000 tokens  Context left until auto-compact: 30%";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(42000));
        assert_eq!(parsed.bashes, Some(3));
        assert_eq!(parsed.compact_remaining, Some(30));
    }

    #[test]
    fn test_parse_status_bar_extreme_wrap_incident_2026_04_18() {
        // 2026-04-18 21:23 ET — extremely narrow tmux pane ate the usual
        // "bypass permissi" and "-- INSERT --" indicators by splitting them
        // across multiple LOGICAL lines (not just visual wraps that -J would
        // rejoin). The pane tail captured by parse_miss_tail reads:
        //     partial response | received | ───── | ❯ | ───── |
        //     --   ⏵⏵ bypass | INSERT | -- | 606746 tokens | ◉ xhigh · /effort
        //
        // Previously parse_status_bar returned tokens=None because
        // has_status_bar couldn't match any line: "bypass" stood alone
        // (no "permissi"), "INSERT" stood alone (no dashes), no "shells" /
        // "background tasks" / "auto-compact" keyword anywhere. The daemon
        // then spuriously flagged dead_checks=4 even though the pane
        // clearly showed "606746 tokens". Andrew pkilled tmux at 21:24 ET
        // because the main loop was unresponsive and no alert had fired.
        //
        // The fix: recognize `⏵⏵` (the permission-mode icon, unique to the
        // status bar) as a status-bar indicator. It is always present when
        // the bar is rendered with `bypass` or `accept edits` permissions,
        // regardless of how narrowly the terminal wraps the adjacent text.
        let input = "\
                     partial response\n\
                     received\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                     \u{276f}\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                     --      \u{23f5}\u{23f5} bypass\n\
                     INSERT\n\
                     --\n\
                     606746 tokens\n\
                     \u{25c9} xhigh \u{00b7} /effort";
        let parsed = parse_status_bar(input);
        assert_eq!(
            parsed.tokens,
            Some(606746),
            "status bar with only ⏵⏵ icon (no \"permissi\" / \"INSERT --\" substrings \
             on any single line) must still be recognized — this was the 2026-04-18 \
             incident where Andrew killed tmux"
        );
    }

    #[test]
    fn test_parse_status_bar_accept_edits_icon_alone() {
        // Similar to the wrap incident but with a narrower wrap that splits
        // even the emoji from its words. `⏵⏵` + a tokens line on its own
        // must be enough.
        let input = "some chat output\n\
                     \u{23f5}\u{23f5}\n\
                     128000 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(128000));
    }

    #[test]
    fn test_parse_status_bar_singular_shell() {
        // Real status bar from 2026-04-27 00:16Z parse miss: status bar emits
        // "1 shell" (singular), not "1 shells". Previously the bash_re was
        // anchored on `(?:bashes|background\s+tasks|shells)\b` (plural-only),
        // so the count was lost AND `has_status_bar` failed to detect the
        // bar via the " shells" substring → tokens were also unparseable on
        // a normal status-bar-suffix line.
        let input = "some output\n\
                     -- INSERT -- \u{23f5}\u{23f5} bypass permissions on \u{00b7} 1 shell \u{00b7} 50000 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(1));
        assert_eq!(parsed.tokens, Some(50000));
    }

    #[test]
    fn test_parse_status_bar_singular_background_task() {
        let input = "-- INSERT -- 1 background task";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(1));
    }

    #[test]
    fn test_parse_status_bar_singular_bash() {
        let input = "-- INSERT -- 1 bash";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(1));
    }

    #[test]
    fn test_parse_status_bar_overlay_active_shells() {
        // Real overlay panel from 2026-04-27 01:57Z parse miss: when the
        // user presses ctrl+b to view the Background-tasks panel, the
        // status bar is replaced with a panel that reads:
        //     Background tasks
        //     4 active shells
        //     watcher-ctl run alerts-watcher (running)
        //     ...
        // Previously bash_re didn't tolerate the "active" qualifier, so the
        // count was lost AND has_status_bar didn't detect the panel, so
        // even the thinking-indicator token form was suppressed.
        let input = "\
                     \u{25cf} Newspapering\u{2026} (21s \u{00b7} \u{2191} 286 tokens \u{00b7} thought for 1s)\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                       Background tasks\n\
                       4 active shells\n\
                         watcher-ctl run alerts-watcher (running)\n\
                         watcher-ctl run claude-event-watch (running)\n\
                         watcher-ctl run memory-remind (running)\n\
                       \u{276f} watcher-ctl run events-watcher (running)\n\
                       \u{2191}/\u{2193} to select \u{00b7} Enter to view \u{00b7} x to stop \u{00b7} \u{2190}/Esc to close";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(parsed.bashes, Some(4));
        // 2026-08 hardening: the "↑ 286 tokens" thinking count is NOT the
        // session total, so `tokens` stays None; the overlay + thinking
        // indicator still register as a status bar (no parse miss).
        assert_eq!(parsed.tokens, None);
        assert!(saw_bar);
    }

    #[test]
    fn test_parse_status_bar_overlay_with_shells_and_agents_section_2026_04_27() {
        // 2026-04-27T03:20:43Z parse-miss reproduction. The Background-tasks
        // overlay introduced a new two-section layout:
        //     Background tasks
        //     3 active shells · 1 active agent
        //       Shells (3)
        //         watcher-ctl run alerts-watcher (running)
        //         watcher-ctl run memory-remind (running)
        //         watcher-ctl run events-watcher (running)
        //       Local agents (1)
        //         Execute startup-context-trim (running)
        //       ↑/↓ to select · Enter to view · ...
        //
        // The overlay is taller than 10 lines AND tmux capture preserves
        // blank lines that parse_miss_tail's diagnostic strips, so the WARN
        // looked like the parser saw the count line — but the parser's
        // bottom-10 window had been pushed past it by intervening blanks.
        // Padding with blanks here reproduces the exact failure mode
        // observed in production.
        let input = "previous chat output\n\
\n\
\u{25cf} Some action\n\
\n\
  \u{2500}\u{2500}\u{2500}\u{2500}\n\
\n\
  Background tasks\n\
\n\
   3 active shells \u{00b7} 1 active agent\n\
\n\
     Shells (3)\n\
\n\
   \u{276f} watcher-ctl run alerts-watcher (running)\n\
     watcher-ctl run memory-remind (running)\n\
     watcher-ctl run events-watcher (running)\n\
     Local agents (1)\n\
     Execute startup-context-trim (running)\n\
   \u{2191}/\u{2193} to select \u{00b7} Enter to view \u{00b7} x to stop \u{00b7} ctrl+x ctrl+k to stop all agents \u{00b7} \u{2190}/Esc\n\
   to close";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(
            parsed.bashes,
            Some(3),
            "overlay layout: 3 active shells must be extracted from full pane scan"
        );
        assert!(
            saw_bar,
            "overlay markers (Background tasks / active shells / Local agents) \
             must register as status-bar visible to suppress is_parse_miss"
        );
    }

    #[test]
    fn test_parse_status_bar_overlay_thinking_token_pushed_above_window() {
        // 2026-04-27T01:59:17Z parse-miss reproduction. An idle status bar
        // (no counts) is at the bottom of the pane, but a thinking line
        // (`↓ 1.3k tokens`) is more than 10 lines above. Previously the
        // parser only scanned the bottom 10 lines for token_thinking_re
        // and missed it.
        let input = "\u{25cf} Background command \"Restart memory-remind\" failed with exit code 1\n\
\u{25cf} Background command \"Restart claude-event-watch\" failed with exit code 1\n\
\u{25cf} Read(/home/hndrewaall/.claude/projects/-home-hndrewaall/e34f3a78-8c8e-4b5b-b2c6-7cd0a32684a2/tool-results/bgvq1ijn1.txt)\n\
  \u{239d}  Read 244 lines\n\
\u{25cf} Zigzagging\u{2026} (37s \u{00b7} \u{2193} 1.3k tokens \u{00b7} thought for 13s)\n\
\n\
\n\
\n\
\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        // 2026-08 hardening: the whole-pane fallback still RECOGNIZES a
        // thinking indicator above the 10-line window (so it is not a parse
        // miss), but no longer TRUSTS its count as the session total.
        assert_eq!(
            parsed.tokens, None,
            "a thinking indicator above the window is a recognized UI state, \
             but its count is the current turn's, not the session total"
        );
        assert!(
            saw_bar,
            "thinking indicator / ⏵⏵ icon must register as status bar"
        );
    }

    #[test]
    fn test_parse_status_bar_overlay_active_shell_singular() {
        // Defensive: overlay with "1 active shell" (singular) should also
        // parse correctly via the whole-pane scan.
        let input = "previous output\n\
\n\
\n\
\n\
\n\
\n\
  Background tasks\n\
\n\
   1 active shell\n\
\n\
     Shells (1)\n\
\n\
     watcher-ctl run alerts-watcher (running)\n\
   \u{2191}/\u{2193} to select \u{00b7} Enter to view \u{00b7} x to stop \u{00b7} \u{2190}/Esc to close";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(1));
    }

    #[test]
    fn test_parse_status_bar_overlay_does_not_overshadow_inline_bar() {
        // When BOTH an overlay-looking line AND an inline status bar are
        // present (defensive — shouldn't really happen), the inline bar's
        // shell count (the bottom-10 hit) takes precedence: bash_re's
        // first-match-wins via assigning to result.bashes inside the loop,
        // and the overlay-fallback only runs if result.bashes is None.
        let input = "  Background tasks\n\
   3 active shells \u{00b7} 1 active agent\n\
\n\
\n\
\n\
\n\
\n\
\n\
\n\
\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{23f5}\u{23f5} bypass permissions on \u{00b7} 7 shells \u{00b7} 999 tokens";
        let parsed = parse_status_bar(input);
        // Inline bar wins.
        assert_eq!(parsed.bashes, Some(7));
        assert_eq!(parsed.tokens, Some(999));
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_k_suffix() {
        // Real thinking line from 2026-04-27 00:16Z parse miss: when the
        // status bar is partly obscured but a thinking line is visible, we
        // can still extract a token count from "↑ 2.3k tokens".
        let input = "\u{25cf} Honking\u{2026} (1m 9s \u{00b7} \u{2191} 2.3k tokens)";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        // 2026-08 hardening: a thinking indicator's token count is the CURRENT
        // TURN's own output, never the session context total, so it must NOT
        // populate `tokens` (a `2.3k` reading here is exactly the tiny misparse
        // that manufactured phantom context clears). It IS a recognized UI
        // state, so it still suppresses `is_parse_miss`.
        assert_eq!(parsed.tokens, None);
        assert!(saw_bar);
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_down_arrow() {
        // Some thinking lines use ↓ instead of ↑.
        let input = "\u{25cf} Zigzagging\u{2026} (37s \u{00b7} \u{2193} 1.3k tokens \u{00b7} thought for 13s)";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        // Down-arrow thinking indicator: still the current turn's count, not
        // the session total -- refused (see k-suffix test).
        assert_eq!(parsed.tokens, None);
        assert!(saw_bar);
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_no_suffix() {
        // ↑ N tokens (no suffix) — N is a literal integer.
        let input = "\u{25cf} Newspapering\u{2026} (21s \u{00b7} \u{2191} 286 tokens \u{00b7} thought for 1s)";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        // A bare-integer (`286`) thinking count is the classic sub-1000 tiny
        // misparse; it must not become the session total.
        assert_eq!(parsed.tokens, None);
        assert!(saw_bar);
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_m_suffix() {
        // Defensive: M-suffix support for huge contexts (1.4M tokens).
        let input = "\u{25cf} Cooking\u{2026} (5m \u{00b7} \u{2191} 1.4M tokens)";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        // Even a huge M-suffixed thinking count is the current turn's, not the
        // session context total -- refused all the same.
        assert_eq!(parsed.tokens, None);
        assert!(saw_bar);
    }

    /// A genuine low-token fresh session (a real BARE context total, no arrow
    /// prefix) MUST still be detected -- the hardening only refuses
    /// thinking-indicator / roster numbers, never a real bare total, so
    /// fresh-/clear detection in the low-token window is preserved.
    #[test]
    fn test_genuine_low_token_fresh_session_still_detected() {
        let input = "\u{23f5}\u{23f5} bypass permissions on \u{00b7} 0 shells \u{00b7} 1,200 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(1200));
    }

    /// Companion to the "both refused" tests: when a real bare context total
    /// AND a thinking indicator are both on screen, the bare total wins and the
    /// thinking count is ignored.
    #[test]
    fn test_bare_total_wins_over_thinking_indicator() {
        let input = "\
\u{2733} Boogieing\u{2026} (4s \u{00b7} \u{2193} 26000 tokens)\n\
\u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 shells \u{00b7} 224598 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(224598));
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_does_not_match_prose() {
        // Thinking-indicator regex is anchored on the ↑/↓ arrow + suffix
        // combination, so prose mentioning "tokens" without that anchor
        // must not match (we don't want to read a token count out of chat
        // text).
        let input = "Hey, that took about 500 tokens to compute.\n\
                     \u{276f}";
        let parsed = parse_status_bar(input);
        assert_eq!(
            parsed.tokens, None,
            "must not match token counts in chat prose"
        );
    }

    #[test]
    fn test_parse_status_bar_idle_no_counts() {
        // A truly idle status bar — no shells, no tokens. The bar is
        // visible but neither count is rendered. parse_status_bar returns
        // Nones for both, but parse_status_bar_with_diag should report
        // saw_status_bar=true, which suppresses the parse-miss warning.
        let input = "\u{2500}\u{2500}\u{2500}\u{2500}\n\
                     \u{276f}\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\n\
                     \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(parsed.tokens, None);
        assert_eq!(parsed.bashes, None);
        assert!(saw_bar, "⏵⏵ icon must register as a status-bar marker");
    }

    #[test]
    fn test_parse_status_bar_diag_no_status_bar_visible() {
        // Pane content with no status-bar markers anywhere — saw_status_bar
        // must be false so is_parse_miss correctly flags this as suspicious.
        let input = "hello world\nno status bar here";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(parsed.tokens, None);
        assert_eq!(parsed.bashes, None);
        assert!(!saw_bar);
    }

    #[test]
    fn test_parse_status_bar_diag_full_bar_returns_true() {
        let input = "\u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 shells \u{00b7} 100 tokens";
        let (parsed, saw_bar) = parse_status_bar_with_diag(input);
        assert_eq!(parsed.tokens, Some(100));
        assert_eq!(parsed.bashes, Some(5));
        assert!(saw_bar);
    }

    #[test]
    fn test_parse_status_bar_single_chevron_not_enough() {
        // A lone `>` or the prompt character `❯` isn't a status-bar marker —
        // Claude's chat output frequently contains chevrons. We do NOT want
        // to widen the indicator set so far that we match prose that happens
        // to mention "500 tokens" somewhere.
        let input = "Hey, cost about 500 tokens per request.\n\
                     \u{276f}";
        let parsed = parse_status_bar(input);
        assert_eq!(
            parsed.tokens, None,
            "must not match token counts in chat prose just because the \
             prompt char is visible"
        );
    }

    // --- is_parse_miss tests ---

    #[test]
    fn test_is_parse_miss_empty_capture() {
        // Empty pane capture is "process gone", not a parse miss.
        let parsed = ParsedStatusBar::default();
        assert!(!is_parse_miss("", &parsed, false));
        assert!(!is_parse_miss("   \n\t\n  ", &parsed, false));
    }

    #[test]
    fn test_is_parse_miss_has_content_but_nothing_parsed() {
        // Non-empty pane with no tokens/bashes AND no status bar marker is
        // the suspicious case.
        let parsed = ParsedStatusBar::default();
        assert!(is_parse_miss(
            "hello world\nno status bar here",
            &parsed,
            false
        ));
    }

    #[test]
    fn test_is_parse_miss_status_bar_visible_no_counts() {
        // Status bar IS visible but has no shell/token counts (legitimately
        // idle: 0 shells, 0 tokens displayed). This must NOT be flagged as
        // a parse miss — there is nothing for the parser to harden against.
        let parsed = ParsedStatusBar::default();
        let pane = "some chat output\n\
                     \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt";
        assert!(!is_parse_miss(pane, &parsed, true));
    }

    #[test]
    fn test_is_parse_miss_tokens_found() {
        // Any successful parse = not a miss.
        let parsed = ParsedStatusBar {
            tokens: Some(100),
            bashes: None,
            compact_remaining: None,
        };
        assert!(!is_parse_miss("some content", &parsed, false));
        assert!(!is_parse_miss("some content", &parsed, true));
    }

    #[test]
    fn test_is_parse_miss_bashes_found() {
        let parsed = ParsedStatusBar {
            tokens: None,
            bashes: Some(3),
            compact_remaining: None,
        };
        assert!(!is_parse_miss("some content", &parsed, false));
        assert!(!is_parse_miss("some content", &parsed, true));
    }

    // --- parse_miss_tail tests ---

    #[test]
    fn test_parse_miss_tail_basic() {
        let input = "line1\nline2\nline3\nline4";
        let tail = parse_miss_tail(input, 2, 100);
        assert_eq!(tail, "line3 | line4");
    }

    #[test]
    fn test_parse_miss_tail_truncates_long_lines() {
        let long = "x".repeat(500);
        let input = format!("short\n{}", long);
        let tail = parse_miss_tail(&input, 5, 50);
        assert!(tail.contains("short"));
        assert!(tail.contains("…"));
        let segments: Vec<&str> = tail.split(" | ").collect();
        assert_eq!(segments.len(), 2);
        // Truncated segment = 50 chars + ellipsis
        assert!(segments[1].chars().count() <= 51);
    }

    #[test]
    fn test_parse_miss_tail_skips_blank_lines() {
        let input = "keep1\n\n   \nkeep2\n\nkeep3";
        let tail = parse_miss_tail(input, 10, 100);
        assert_eq!(tail, "keep1 | keep2 | keep3");
    }

    #[test]
    fn test_parse_miss_tail_fewer_lines_than_max() {
        let tail = parse_miss_tail("one\ntwo", 10, 100);
        assert_eq!(tail, "one | two");
    }

    // --- extract_version_from_path tests ---

    #[test]
    fn test_is_version_string_comm() {
        // Native-installer comm: bare semver version string.
        assert!(is_version_string_comm("2.1.217"));
        assert!(is_version_string_comm("2.1.77"));
        assert!(is_version_string_comm("10.20.30"));
        assert!(is_version_string_comm("1.2.3.4"));
        // NOT version strings — ordinary comms / partials must be rejected so
        // the fast-path only matches the native-installer pane.
        assert!(!is_version_string_comm("claude"));
        assert!(!is_version_string_comm("node"));
        assert!(!is_version_string_comm("bash"));
        assert!(!is_version_string_comm("2.1")); // only one dot
        assert!(!is_version_string_comm("2.1.")); // trailing empty component
        assert!(!is_version_string_comm("v2.1.7")); // non-digit prefix
        assert!(!is_version_string_comm("2.1.7a")); // non-digit component
        assert!(!is_version_string_comm(""));
    }

    #[test]
    fn test_extract_version_simple() {
        let path = "/home/user/.local/share/claude/versions/2.1.77/node_modules/.bin/claude";
        assert_eq!(extract_version_from_path(path), Some("2.1.77".to_string()));
    }

    #[test]
    fn test_extract_version_three_part() {
        let path = "/opt/versions/1.0.0/bin/claude";
        assert_eq!(extract_version_from_path(path), Some("1.0.0".to_string()));
    }

    #[test]
    fn test_extract_version_no_match() {
        let path = "/usr/bin/claude";
        assert_eq!(extract_version_from_path(path), None);
    }

    #[test]
    fn test_extract_version_empty() {
        assert_eq!(extract_version_from_path(""), None);
    }

    // --- extract_version_from_json tests ---

    #[test]
    fn test_extract_version_from_json_package_json() {
        // Shape of @anthropic-ai/claude-code package.json.
        let json = r#"{
  "name": "@anthropic-ai/claude-code",
  "version": "2.1.178",
  "bin": { "claude": "bin/claude.exe" }
}"#;
        assert_eq!(
            extract_version_from_json(json),
            Some("2.1.178".to_string())
        );
    }

    #[test]
    fn test_extract_version_from_json_session_marker() {
        // Shape of ~/.claude/sessions/<PID>.json (the running-version source
        // for the npm-global layout).
        let json = r#"{"pid":68,"sessionId":"5d5f5863","cwd":"/repos","version":"2.1.175","kind":"interactive","status":"busy"}"#;
        assert_eq!(
            extract_version_from_json(json),
            Some("2.1.175".to_string())
        );
    }

    #[test]
    fn test_extract_version_from_json_whitespace_variants() {
        assert_eq!(
            extract_version_from_json(r#"{ "version" : "1.0.0" }"#),
            Some("1.0.0".to_string())
        );
    }

    #[test]
    fn test_extract_version_from_json_no_version() {
        assert_eq!(extract_version_from_json(r#"{"name":"x"}"#), None);
    }

    #[test]
    fn test_extract_version_from_json_non_numeric_ignored() {
        // A "version" that isn't a numeric semver (e.g. an unrelated field)
        // must not be mistaken for the package version.
        assert_eq!(
            extract_version_from_json(r#"{"version":"latest"}"#),
            None
        );
    }

    // --- is_claude_hooks_shim tests ---

    #[test]
    fn test_is_claude_hooks_shim() {
        // The container's shim wrapper paths.
        assert!(is_claude_hooks_shim(std::path::Path::new(
            "/usr/local/lib/claude-hooks-shim/claude"
        )));
        assert!(is_claude_hooks_shim(std::path::Path::new(
            "/usr/local/lib/claude-hooks-shim/claude-mcp-settings-shim"
        )));
        // The real npm-global binary is NOT a shim.
        assert!(!is_claude_hooks_shim(std::path::Path::new(
            "/home/u/.npm-global/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"
        )));
        // The native versioned binary is NOT a shim.
        assert!(!is_claude_hooks_shim(std::path::Path::new(
            "/home/u/.local/share/claude/versions/2.1.186/node_modules/.bin/claude"
        )));
    }

    // --- pane-scoped running-version selection tests ---

    #[test]
    fn test_compare_versions_numeric_not_lexical() {
        use std::cmp::Ordering;
        // Lexical compare would call "2.1.99" > "2.1.245"; numeric must not.
        assert_eq!(compare_versions("2.1.245", "2.1.99"), Ordering::Greater);
        assert_eq!(compare_versions("2.1.243", "2.1.245"), Ordering::Less);
        assert_eq!(compare_versions("2.1.245", "2.1.245"), Ordering::Equal);
        assert_eq!(compare_versions("2.2.0", "2.1.999"), Ordering::Greater);
    }

    #[test]
    fn test_is_claude_tui_exe() {
        // Native versioned layout, launcher symlink, npm-global binary.
        assert!(is_claude_tui_exe(
            "/home/u/.local/share/claude/versions/2.1.245/node_modules/.bin/claude"
        ));
        assert!(is_claude_tui_exe("/home/u/.local/bin/claude"));
        assert!(is_claude_tui_exe(
            "/home/u/.npm-global/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"
        ));
        // A deleted-inode exe target keeps the /versions/ segment.
        assert!(is_claude_tui_exe(
            "/home/u/.local/share/claude/versions/2.1.245/cli.js (deleted)"
        ));
        // NOT claude TUIs — these merely match `pgrep -af claude`.
        assert!(!is_claude_tui_exe("/usr/local/bin/claude-watch"));
        assert!(!is_claude_tui_exe("/usr/bin/tmux"));
        assert!(!is_claude_tui_exe("/usr/bin/bash"));
    }

    #[test]
    fn test_select_running_version_prefers_pane_pid() {
        // The live main-loop TUI is pid 143 on 2.1.245; an orphaned OLD build
        // (pid 99 on 2.1.241) is still executable and also resolved. The pane
        // PID must win regardless of scan order.
        let candidates = vec![
            RunningCandidate { pid: "99".into(), version: "2.1.241".into() },
            RunningCandidate { pid: "143".into(), version: "2.1.245".into() },
        ];
        assert_eq!(
            select_running_version(&candidates, Some("143")),
            Some("2.1.245".to_string())
        );
    }

    #[test]
    fn test_select_running_version_pane_pid_even_if_lower() {
        // Truthfulness: if the pane's OWN pid loaded an older build (genuinely
        // running behind installed), report THAT — a real mismatch the updater
        // should act on — not some other process's newer version.
        let candidates = vec![
            RunningCandidate { pid: "143".into(), version: "2.1.241".into() },
            RunningCandidate { pid: "200".into(), version: "2.1.245".into() },
        ];
        assert_eq!(
            select_running_version(&candidates, Some("143")),
            Some("2.1.241".to_string())
        );
    }

    #[test]
    fn test_select_running_version_fallback_highest_when_pane_pid_absent() {
        // Pane PID unresolved / not among candidates: a dying OLD process
        // (2.1.241) must NOT mask the live NEW one (2.1.245). Highest wins —
        // the exact orphan-masks-live failure mode of the global first-match.
        let candidates = vec![
            RunningCandidate { pid: "99".into(), version: "2.1.241".into() },
            RunningCandidate { pid: "143".into(), version: "2.1.245".into() },
            RunningCandidate { pid: "150".into(), version: "2.1.243".into() },
        ];
        assert_eq!(
            select_running_version(&candidates, None),
            Some("2.1.245".to_string())
        );
        // Pane PID given but not in the candidate set -> same highest-wins path.
        assert_eq!(
            select_running_version(&candidates, Some("777")),
            Some("2.1.245".to_string())
        );
    }

    #[test]
    fn test_select_running_version_empty_is_none() {
        assert_eq!(select_running_version(&[], Some("143")), None);
        assert_eq!(select_running_version(&[], None), None);
    }

    // --- resolve_installed_version tests ---

    #[test]
    fn test_resolve_installed_version_native_layout() {
        // Native versioned-symlink layout: a symlink whose canonical target
        // contains /versions/X.Y.Z/ — version comes straight from the path.
        let tmp = std::env::temp_dir().join(format!("cw-native-{}", std::process::id()));
        let versions = tmp.join(".local/share/claude/versions/2.1.77/node_modules/.bin");
        std::fs::create_dir_all(&versions).unwrap();
        let real_bin = versions.join("claude");
        std::fs::write(&real_bin, b"#!/bin/sh\n").unwrap();
        let bindir = tmp.join(".local/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let link = bindir.join("claude");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real_bin, &link).unwrap();

        assert_eq!(
            resolve_installed_version(&link),
            Some("2.1.77".to_string())
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_resolve_installed_version_npm_global_layout() {
        // npm-global layout: launcher -> .../@anthropic-ai/claude-code/bin/claude.exe,
        // no version in the path. Version must come from package.json.
        let tmp = std::env::temp_dir().join(format!("cw-npm-{}", std::process::id()));
        let pkg = tmp.join("lib/node_modules/@anthropic-ai/claude-code");
        let bin = pkg.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@anthropic-ai/claude-code","version":"2.1.178"}"#,
        )
        .unwrap();
        let real_bin = bin.join("claude.exe");
        std::fs::write(&real_bin, b"#!/bin/sh\n").unwrap();
        let bindir = tmp.join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let link = bindir.join("claude");
        let _ = std::fs::remove_file(&link);
        // Mirror the real npm symlink: ../lib/node_modules/.../bin/claude.exe
        std::os::unix::fs::symlink(&real_bin, &link).unwrap();

        assert_eq!(
            resolve_installed_version(&link),
            Some("2.1.178".to_string())
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_resolve_installed_version_unrelated_package_json_ignored() {
        // A package.json that isn't @anthropic-ai/claude-code must not be
        // trusted as the source of the claude version.
        let tmp = std::env::temp_dir().join(format!("cw-other-{}", std::process::id()));
        let bin = tmp.join("some-tool/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            tmp.join("some-tool/package.json"),
            r#"{"name":"some-other-tool","version":"9.9.9"}"#,
        )
        .unwrap();
        let real_bin = bin.join("claude.exe");
        std::fs::write(&real_bin, b"#!/bin/sh\n").unwrap();

        // Canonicalize directly (no symlink) — walk up finds the unrelated
        // package.json but rejects it; no native version in path either.
        assert_eq!(resolve_installed_version(&real_bin), None);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_resolve_installed_version_missing_binary() {
        let missing = std::path::PathBuf::from("/nonexistent/path/to/claude");
        assert_eq!(resolve_installed_version(&missing), None);
    }

    // --- parse_watchers_config tests ---

    #[test]
    fn test_parse_watchers_basic() {
        let config = "alerts-watcher|alerts-watcher$|1|true|watcher-ctl run alerts-watcher\n\
                       torrent-wait|torrent-wait$|1|true|watcher-ctl run torrent-wait";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alerts-watcher");
        assert_eq!(entries[0].pattern, "alerts-watcher$");
        assert_eq!(entries[0].min_count, 1);
        assert!(entries[0].enabled);
        assert_eq!(
            entries[0].start_cmd.as_deref(),
            Some("watcher-ctl run alerts-watcher")
        );
        assert_eq!(entries[1].name, "torrent-wait");
        assert_eq!(
            entries[1].start_cmd.as_deref(),
            Some("watcher-ctl run torrent-wait")
        );
        // Sixth pipe-separated field is optional on_restart_cmd; default None.
        assert!(entries[0].on_restart_cmd.is_none());
    }

    #[test]
    fn test_parse_watchers_on_restart_cmd() {
        let config = "demo|demo$|1|true|run-demo|history-dump --since 5m";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].on_restart_cmd.as_deref(),
            Some("history-dump --since 5m"),
        );
    }

    #[test]
    fn test_parse_watchers_disabled() {
        let config = "watcher-a|pattern-a|1|false|cmd-a";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].enabled);
        assert_eq!(entries[0].start_cmd.as_deref(), Some("cmd-a"));
    }

    #[test]
    fn test_parse_watchers_comments_and_blanks() {
        let config = "# This is a comment\n\
                       \n\
                       watcher-a|pattern-a|2|true|cmd-a\n\
                       # Another comment\n\
                       \n";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "watcher-a");
        assert_eq!(entries[0].min_count, 2);
    }

    #[test]
    fn test_parse_watchers_minimal_fields() {
        let config = "watcher-a|pattern-a";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].min_count, 1); // default
        assert!(entries[0].enabled); // default
        assert_eq!(entries[0].start_cmd, None); // no start_cmd
    }

    #[test]
    fn test_parse_watchers_single_field_rejected() {
        let config = "just-a-name";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_parse_watchers_empty() {
        let entries = parse_watchers_config_str("");
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_parse_watchers_invalid_min_count() {
        let config = "watcher-a|pattern-a|notanumber|true|cmd-a";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].min_count, 1); // falls back to default
    }

    #[test]
    fn test_parse_watchers_config_missing_file() {
        let entries = parse_watchers_config("/tmp/nonexistent-watchers-test.conf");
        assert_eq!(entries.len(), 0);
    }

    // --- mode field + layered override ------------------------------------

    #[test]
    fn test_parse_watchers_mode_defaults_to_oneshot() {
        let entries = parse_watchers_config_str("evw|bin/evw|1|true|evw --quiet 10");
        assert_eq!(entries[0].mode, WatcherMode::Oneshot);
        assert_eq!(entries[0].layer, WATCHER_LAYER_BASE);
        assert!(entries[0].overridden.is_empty());
        assert_eq!(
            entries[0].effective_monitor_cmd().as_deref(),
            Some("evw --quiet 10 --mode monitor")
        );
    }

    #[test]
    fn test_parse_watchers_mode_positional_seventh_field() {
        let entries =
            parse_watchers_config_str("evw|bin/evw|1|true|evw --quiet 10|hist|monitor|evw --stream");
        assert_eq!(entries[0].mode, WatcherMode::Monitor);
        assert_eq!(entries[0].on_restart_cmd.as_deref(), Some("hist"));
        assert_eq!(entries[0].monitor_cmd.as_deref(), Some("evw --stream"));
        assert_eq!(entries[0].effective_monitor_cmd().as_deref(), Some("evw --stream"));
        // Blank on_restart_cmd slot still lets mode land in slot 7.
        let entries = parse_watchers_config_str("evw|bin/evw|1|true|evw||monitor");
        assert_eq!(entries[0].mode, WatcherMode::Monitor);
        assert!(entries[0].on_restart_cmd.is_none());
    }

    #[test]
    fn test_parse_watchers_mode_keyed_form() {
        let entries = parse_watchers_config_str("evw|bin/evw|mode=monitor|enabled=false");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pattern, "bin/evw");
        assert_eq!(entries[0].mode, WatcherMode::Monitor);
        assert!(!entries[0].enabled);
        // Keyed fields do not consume positional slots: min_count stays default.
        assert_eq!(entries[0].min_count, 1);
        // A start_cmd that happens to contain `=` is NOT mistaken for a key.
        let entries = parse_watchers_config_str("evw|bin/evw|1|true|evw --debounce=60");
        assert_eq!(entries[0].start_cmd.as_deref(), Some("evw --debounce=60"));
    }

    #[test]
    fn test_parse_watchers_mode_unknown_value_falls_back_to_oneshot() {
        let entries = parse_watchers_config_str("evw|bin/evw|1|true|evw||streamy");
        assert_eq!(entries[0].mode, WatcherMode::Oneshot);
        assert_eq!(WatcherMode::parse("exit"), Some(WatcherMode::Oneshot));
        assert_eq!(WatcherMode::parse("one-shot"), Some(WatcherMode::Oneshot));
        assert_eq!(WatcherMode::parse(" Monitor "), Some(WatcherMode::Monitor));
        assert_eq!(WatcherMode::parse("bogus"), None);
    }

    #[test]
    fn test_merge_override_changes_only_set_fields() {
        let base = parse_watchers_config_str(
            "evw|bin/evw|1|true|evw --quiet 10\nsig|--tag dm|1|true|signal-wait --dm\n",
        );
        let merged = merge_watchers_override_str(base, "# flip evw to monitor\nevw|mode=monitor\n");
        assert_eq!(merged.len(), 2);
        let evw = &merged[0];
        assert_eq!(evw.mode, WatcherMode::Monitor);
        // Everything the override did not mention is inherited verbatim.
        assert_eq!(evw.pattern, "bin/evw");
        assert!(evw.enabled);
        assert_eq!(evw.start_cmd.as_deref(), Some("evw --quiet 10"));
        assert_eq!(evw.layer, WATCHER_LAYER_BASE);
        assert_eq!(evw.overridden, vec!["mode".to_string()]);
        // Untouched sibling is pristine.
        assert!(merged[1].overridden.is_empty());
        assert_eq!(merged[1].mode, WatcherMode::Oneshot);
    }

    #[test]
    fn test_merge_override_positional_blank_means_inherit() {
        let base = parse_watchers_config_str("evw|bin/evw|1|true|evw --quiet 10|hist\n");
        // Positional override: blank pattern/min_count/start_cmd/on_restart
        // inherit; only enabled (slot 3) + mode (slot 6) are set.
        let merged = merge_watchers_override_str(base, "evw||| false|||monitor\n");
        let evw = &merged[0];
        assert_eq!(evw.pattern, "bin/evw");
        assert_eq!(evw.min_count, 1);
        assert!(!evw.enabled);
        assert_eq!(evw.start_cmd.as_deref(), Some("evw --quiet 10"));
        assert_eq!(evw.on_restart_cmd.as_deref(), Some("hist"));
        assert_eq!(evw.mode, WatcherMode::Monitor);
        let mut ov = evw.overridden.clone();
        ov.sort();
        assert_eq!(ov, vec!["enabled".to_string(), "mode".to_string()]);
    }

    #[test]
    fn test_merge_override_appends_unknown_watcher_and_later_lines_win() {
        let base = parse_watchers_config_str("evw|bin/evw|1|true|evw\n");
        let merged = merge_watchers_override_str(
            base,
            "extra|bin/extra|1|true|extra-watch||monitor\nevw|mode=monitor\nevw|mode=oneshot\n",
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].name, "extra");
        assert_eq!(merged[1].layer, WATCHER_LAYER_OVERRIDE);
        assert_eq!(merged[1].mode, WatcherMode::Monitor);
        // Last line naming evw wins.
        assert_eq!(merged[0].mode, WatcherMode::Oneshot);
        assert_eq!(merged[0].overridden, vec!["mode".to_string()]);
    }

    #[test]
    fn test_load_watchers_config_layers_and_absent_override() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("watchers.conf");
        std::fs::write(&base, "evw|bin/evw|1|true|evw --quiet 10\n").unwrap();
        let ov = dir.path().join("watchers.override.conf");

        // Override absent: the committed default loads on its own.
        let entries = load_watchers_config(base.to_str().unwrap(), Some(ov.to_str().unwrap()));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mode, WatcherMode::Oneshot);

        // Override present: it wins on the fields it sets.
        std::fs::write(&ov, "evw|mode=monitor|enabled=false\n").unwrap();
        let entries = load_watchers_config(base.to_str().unwrap(), Some(ov.to_str().unwrap()));
        assert_eq!(entries[0].mode, WatcherMode::Monitor);
        assert!(!entries[0].enabled);

        // Override reached THROUGH A SYMLINK (the "linked in from a repo" shape).
        let repo_file = dir.path().join("repo-watchers.override.conf");
        std::fs::write(&repo_file, "evw|mode=monitor\n").unwrap();
        let link = dir.path().join("linked.override.conf");
        std::os::unix::fs::symlink(&repo_file, &link).unwrap();
        let entries = load_watchers_config(base.to_str().unwrap(), Some(link.to_str().unwrap()));
        assert_eq!(entries[0].mode, WatcherMode::Monitor);
        assert!(entries[0].enabled, "symlinked override set only mode");

        // Dangling symlink (target outside the mounted tree) == absent.
        let dangling = dir.path().join("dangling.override.conf");
        std::os::unix::fs::symlink(dir.path().join("does-not-exist"), &dangling).unwrap();
        let entries = load_watchers_config(base.to_str().unwrap(), Some(dangling.to_str().unwrap()));
        assert_eq!(entries[0].mode, WatcherMode::Oneshot);
    }

    // --- shared watcher-liveness helpers (hoisted from policy.rs) -----------

    #[test]
    fn test_strip_script_suffix() {
        assert_eq!(strip_script_suffix("claude-event-watch.sh"), "claude-event-watch");
        assert_eq!(strip_script_suffix("x.bash"), "x");
        assert_eq!(strip_script_suffix("y.py"), "y");
        assert_eq!(strip_script_suffix("no-ext"), "no-ext");
        // Only a trailing known extension is stripped.
        assert_eq!(strip_script_suffix("a.sh.txt"), "a.sh.txt");
    }

    #[test]
    fn test_cmdline_matches_exec_transform() {
        // The `.sh` launcher execs the bare binary → cmdline loses `.sh`, must
        // still match (the exec-argv false-DOWN fix).
        let start_cmd = "/opt/claude-container/watchers/claude-event-watch.sh";
        assert!(cmdline_matches_watcher(
            "/bin/bash /usr/local/bin/claude-event-watch",
            start_cmd
        ));
        // Literal (no exec) cmdline still matches.
        assert!(cmdline_matches_watcher(
            "/bin/bash /opt/claude-container/watchers/claude-event-watch.sh",
            start_cmd
        ));
        // Unrelated process rejected.
        assert!(!cmdline_matches_watcher(
            "/usr/bin/python3 /home/u/other-tool.py",
            start_cmd
        ));
        // Empty start_cmd rejected.
        assert!(!cmdline_matches_watcher("/bin/bash /usr/local/bin/x", ""));
        assert!(!cmdline_matches_watcher("/bin/bash /usr/local/bin/x", "   "));
    }

    #[test]
    fn test_pidfile_watcher_is_down_decision() {
        // Live + identity-matched → UP.
        assert!(!pidfile_watcher_is_down(Some(4242), true, true));
        // Missing pidfile → DOWN.
        assert!(pidfile_watcher_is_down(None, false, false));
        assert!(pidfile_watcher_is_down(None, true, true));
        // Stale (dead) recorded PID → DOWN.
        assert!(pidfile_watcher_is_down(Some(4242), false, false));
        // Recycled (alive, cmdline mismatch) → DOWN.
        assert!(pidfile_watcher_is_down(Some(4242), true, false));
    }

    #[test]
    fn test_read_watcher_recorded_pid_prefers_lock_then_pid() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        // No files → None.
        assert_eq!(read_watcher_recorded_pid(d, "evw"), None);
        // Only .pid present → falls back to it.
        std::fs::write(dir.path().join("evw.pid"), "777").unwrap();
        assert_eq!(read_watcher_recorded_pid(d, "evw"), Some(777));
        // .lock present → preferred over .pid.
        std::fs::write(dir.path().join("evw.lock"), "888").unwrap();
        assert_eq!(read_watcher_recorded_pid(d, "evw"), Some(888));
    }

    #[test]
    fn test_is_pid_alive_self_and_bogus() {
        // The test process is, definitionally, alive.
        assert!(is_pid_alive(std::process::id()));
        // A very high PID is essentially guaranteed not to exist. (We do NOT
        // assert on PID 0: `kill(0, ...)` targets the caller's process group
        // and "succeeds", so it is not a meaningful liveness probe — callers
        // that care special-case 0 themselves.)
        assert!(!is_pid_alive(u32::MAX - 1));
    }

    // -------------------------------------------------------------------
    // Multi-dir / multi-file watcher liveness (2026-07 split-brain fix).
    // Detection must find a watcher's liveness file no matter which
    // candidate dir it landed in, and must prefer a FRESH live `.pid` over a
    // STALE dead `.lock` sitting in the same dir. Regression: after a tmux
    // restart put `$XDG_RUNTIME_DIR` in the session env, `signal-wait`'s
    // `.pid` (written by watcher_run to /var/run/claude) was invisible to a
    // reader that resolved to /run/user/<uid> → false-DOWN; and the daemon
    // (no $XDG) picked a stale /var/run/claude `.lock` over the fresh `.pid`
    // → false watcher-down.
    // -------------------------------------------------------------------

    #[test]
    fn pid_dir_candidates_orders_and_dedups() {
        // Both env vars distinct → both, then the fallback.
        assert_eq!(
            pid_dir_candidates(Some("/a"), Some("/b"), None),
            vec!["/a".to_string(), "/b".to_string(), "/var/run/claude".to_string()]
        );
        // XDG only → XDG then fallback.
        assert_eq!(
            pid_dir_candidates(None, Some("/run/user/1000"), None),
            vec!["/run/user/1000".to_string(), "/var/run/claude".to_string()]
        );
        // A value equal to the fallback is de-duplicated, not repeated.
        assert_eq!(
            pid_dir_candidates(Some("/var/run/claude"), None, None),
            vec!["/var/run/claude".to_string()]
        );
        // Identical env values collapse to one entry.
        assert_eq!(
            pid_dir_candidates(Some("/x"), Some("/x"), None),
            vec!["/x".to_string(), "/var/run/claude".to_string()]
        );
        // Empty / whitespace values are skipped.
        assert_eq!(
            pid_dir_candidates(Some(""), Some("   "), None),
            vec!["/var/run/claude".to_string()]
        );
        // Neither set → just the fallback.
        assert_eq!(
            pid_dir_candidates(None, None, None),
            vec!["/var/run/claude".to_string()]
        );
    }

    /// The cron regression: `claude-watch metrics` runs with NO
    /// `$XDG_RUNTIME_DIR`, but the monitor-mode watchers' `.lock` files live in
    /// `/run/user/<uid>`. The uid-derived dir must be scanned regardless of the
    /// reader's env — and must de-dup against an equal `$XDG_RUNTIME_DIR`.
    #[test]
    fn pid_dir_candidates_includes_uid_runtime_dir_without_xdg_env() {
        // No env at all → uid dir, then the fallback.
        assert_eq!(
            pid_dir_candidates(None, None, Some("/run/user/1000")),
            vec!["/run/user/1000".to_string(), "/var/run/claude".to_string()]
        );
        // XDG set to the same dir → one entry, not two.
        assert_eq!(
            pid_dir_candidates(None, Some("/run/user/1000"), Some("/run/user/1000")),
            vec!["/run/user/1000".to_string(), "/var/run/claude".to_string()]
        );
        // XDG pointing elsewhere (a test harness, a container) → env dir
        // first, uid dir still scanned, fallback last.
        assert_eq!(
            pid_dir_candidates(Some("/pids"), Some("/xdg"), Some("/run/user/1000")),
            vec![
                "/pids".to_string(),
                "/xdg".to_string(),
                "/run/user/1000".to_string(),
                "/var/run/claude".to_string()
            ]
        );
        // The live resolver itself always carries the uid dir.
        assert!(
            watcher_pid_dirs().iter().any(|d| d == &uid_runtime_dir()),
            "watcher_pid_dirs() must include {}",
            uid_runtime_dir()
        );
        assert!(uid_runtime_dir().starts_with("/run/user/"));
    }

    #[test]
    fn collect_watcher_recorded_pids_gathers_lock_and_pid_across_dirs() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        // dir A: only a .lock; dir B: only a .pid.
        std::fs::write(a.path().join("w.lock"), "111").unwrap();
        std::fs::write(b.path().join("w.pid"), "222").unwrap();
        let dirs = vec![
            a.path().to_str().unwrap().to_string(),
            b.path().to_str().unwrap().to_string(),
        ];
        assert_eq!(collect_watcher_recorded_pids(&dirs, "w"), vec![111, 222]);

        // Within one dir, .lock is listed before .pid, and a pid repeated
        // across files is de-duplicated (first-seen order preserved).
        let c = tempfile::tempdir().unwrap();
        std::fs::write(c.path().join("w.lock"), "333").unwrap();
        std::fs::write(c.path().join("w.pid"), "333").unwrap();
        let cdirs = vec![c.path().to_str().unwrap().to_string()];
        assert_eq!(collect_watcher_recorded_pids(&cdirs, "w"), vec![333]);
    }

    #[test]
    fn liveness_multi_finds_pid_in_a_second_dir() {
        // The `signal-wait` regression: dir A (would-be $XDG_RUNTIME_DIR) holds
        // NOTHING, dir B (/var/run/claude) holds the live `.pid`. A single-dir
        // reader keyed on dir A saw DOWN; the multi reader finds the live pid
        // in dir B → UP.
        let empty = tempfile::tempdir().unwrap();
        let withpid = tempfile::tempdir().unwrap();
        std::fs::write(
            withpid.path().join("signal-wait-group.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        let dirs = vec![
            empty.path().to_str().unwrap().to_string(),
            withpid.path().to_str().unwrap().to_string(),
        ];
        let (pid, down) = watcher_pidfile_liveness_multi(&dirs, "signal-wait-group", None);
        assert!(!down, "live .pid in the second dir must read as UP");
        assert_eq!(pid, Some(std::process::id()));
    }

    #[test]
    fn liveness_multi_prefers_fresh_live_pid_over_stale_dead_lock() {
        // The daemon `botchat-wait` regression: a STALE `.lock` (dead pid) and a
        // FRESH `.pid` (live pid) coexist in the SAME dir. The old lock-first
        // pick returned the dead pid → false-DOWN; the multi reader considers
        // every recorded pid and picks the alive one → UP.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("botchat-wait.lock"), (u32::MAX - 1).to_string())
            .unwrap();
        std::fs::write(
            dir.path().join("botchat-wait.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        let dirs = vec![dir.path().to_str().unwrap().to_string()];
        let (pid, down) = watcher_pidfile_liveness_multi(&dirs, "botchat-wait", None);
        assert!(!down, "fresh live .pid must win over a stale dead .lock");
        assert_eq!(pid, Some(std::process::id()));
    }

    #[test]
    fn liveness_multi_down_when_all_recorded_pids_dead() {
        // A genuinely dead watcher: every recorded pid is dead → DOWN still
        // fires (so a real restart is triggered). The first recorded pid is
        // returned for diagnostics / `orphaned`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("w.lock"), (u32::MAX - 1).to_string()).unwrap();
        std::fs::write(dir.path().join("w.pid"), (u32::MAX - 2).to_string()).unwrap();
        let dirs = vec![dir.path().to_str().unwrap().to_string()];
        let (pid, down) = watcher_pidfile_liveness_multi(&dirs, "w", None);
        assert!(down, "all-dead recorded pids must read as DOWN");
        assert_eq!(pid, Some(u32::MAX - 1), "first recorded pid surfaced for diagnostics");
    }

    #[test]
    fn liveness_multi_none_when_no_pidfiles() {
        // No liveness file anywhere → DOWN with no recorded pid.
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_str().unwrap().to_string()];
        let (pid, down) = watcher_pidfile_liveness_multi(&dirs, "nope", None);
        assert!(down);
        assert_eq!(pid, None);
    }

    #[test]
    fn runtime_file_age_multi_returns_youngest_across_dirs() {
        let old_dir = tempfile::tempdir().unwrap();
        let fresh_dir = tempfile::tempdir().unwrap();
        // Backdate the file in old_dir well past any plausible young reading.
        std::fs::write(old_dir.path().join("w.lock"), "1").unwrap();
        filetime_set(
            &old_dir.path().join("w.lock"),
            SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        // Fresh file, just written.
        std::fs::write(fresh_dir.path().join("w.pid"), "2").unwrap();
        let dirs = vec![
            old_dir.path().to_str().unwrap().to_string(),
            fresh_dir.path().to_str().unwrap().to_string(),
        ];
        let age = watcher_runtime_file_age_secs_multi(&dirs, "w")
            .expect("at least one runtime file exists");
        assert!(age < 60.0, "must report the youngest (fresh) file's age, got {age}");
    }
}
