//! Claude Code status parsing and watcher/process checks.

use crate::cmd::{run_cmd, run_cmd_any};
use regex_lite::Regex;
use serde::Serialize;
use tracing::{debug, warn};

/// Parsed Claude Code status from tmux pane capture + /proc.
#[derive(Debug, Serialize, Clone)]
pub struct ClaudeStatus {
    pub pane: String,
    pub tokens: u64,
    pub bashes: u64,
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

/// Watcher config entry parsed from watchers.conf.
#[derive(Debug, Clone)]
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
}

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

/// Like `parse_status_bar` but also returns whether a status-bar marker was
/// detected in the tail. The marker flag lets `is_parse_miss` suppress
/// warnings for legitimately-idle status bars (which carry neither tokens
/// nor a shell count).
pub(crate) fn parse_status_bar_with_diag(pane_text: &str) -> (ParsedStatusBar, bool) {
    let mut result = ParsedStatusBar::default();

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
    let bash_re = Regex::new(
        r"(\d+)\s+(?:active\s+)?(?:bash(?:es)?|background\s+task(?:s)?|shell(?:s)?)\b",
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
            if let Some(caps) = token_re.captures(line) {
                if let Some(m) = caps.get(1) {
                    let cleaned = m.as_str().replace(',', "");
                    if let Ok(v) = cleaned.parse::<u64>() {
                        result.tokens = Some(v);
                    }
                }
            }
        }
        // Thinking-indicator tokens (with k/M suffix) — match regardless of
        // status_bar flag. The arrow prefix is itself a strong anchor.
        if result.tokens.is_none() {
            if let Some(caps) = token_thinking_re.captures(line) {
                if let (Some(num), Some(suffix)) = (caps.get(1), caps.get(2)) {
                    if let Ok(base) = num.as_str().parse::<f64>() {
                        let mult: f64 = match suffix.as_str() {
                            "k" | "K" => 1_000.0,
                            "m" | "M" => 1_000_000.0,
                            _ => 1.0,
                        };
                        let v = (base * mult).round() as u64;
                        result.tokens = Some(v);
                    }
                }
            }
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

    // Whole-pane scan for thinking-indicator tokens (always safe because
    // the ↑/↓ + tok anchor never appears in chat prose). Catches cases
    // where a thinking line is more than 10 lines above the bottom.
    if result.tokens.is_none() {
        for line in &lines {
            if let Some(caps) = token_thinking_re.captures(line) {
                if let (Some(num), Some(suffix)) = (caps.get(1), caps.get(2)) {
                    if let Ok(base) = num.as_str().parse::<f64>() {
                        let mult: f64 = match suffix.as_str() {
                            "k" | "K" => 1_000.0,
                            "m" | "M" => 1_000_000.0,
                            _ => 1.0,
                        };
                        let v = (base * mult).round() as u64;
                        result.tokens = Some(v);
                        break;
                    }
                }
            }
        }
    }

    // Treat the overlay as a status-bar marker: even though it visually
    // replaces the bar, it's a known UI state with a count present, and
    // we don't want is_parse_miss to flag it.
    let saw_status_bar = has_status_bar || overlay_visible;

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
    if let Ok(output) = std::process::Command::new("pgrep")
        .args(["-a", "claude"])
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

/// Find the tmux pane running Claude Code.
///
/// Primary: look for `pane_current_command == "claude"`.
/// Fallback: check "bash"/"node" panes for Claude Code status bar content
/// (handles wrapper scripts).
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
            if parts[1] == "claude" {
                return Some(parts[0].to_string());
            }
            if parts[1] == "bash" || parts[1] == "node" {
                candidates.push(parts[0].to_string());
            }
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
/// Falls back to shelling out to `claude-status --json` if native pane discovery
/// fails or if `CLAUDE_STATUS_CMD` env var is set (for test environments).
pub async fn get_claude_status() -> Option<ClaudeStatus> {
    // If CLAUDE_STATUS_CMD is set (test mode), skip native discovery and use fallback
    if std::env::var("CLAUDE_STATUS_CMD").is_ok() {
        debug!("CLAUDE_STATUS_CMD set, using fallback");
        return get_claude_status_fallback().await;
    }

    // Try native pane discovery first
    if let Some(pane) = find_claude_pane().await {
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

/// Pure function: parse watchers config from a string.
pub(crate) fn parse_watchers_config_str(content: &str) -> Vec<WatcherEntry> {
    content
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 2 {
                return None;
            }
            let name = parts[0].to_string();
            let pattern = parts[1].to_string();
            let min_count = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            let enabled = parts.get(3).map(|s| *s == "true").unwrap_or(true);
            let start_cmd = parts
                .get(4)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let on_restart_cmd = parts
                .get(5)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(WatcherEntry {
                name,
                pattern,
                min_count,
                enabled,
                start_cmd,
                on_restart_cmd,
            })
        })
        .collect()
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

/// Read a watcher PID file (`<name>.pid`) and return the recorded PID, if the
/// file exists and contains a parseable integer. Whitespace is trimmed.
/// `None` on missing / unreadable / non-numeric content.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(4));
        // Thinking-indicator token recovery: "↑ 286 tokens".
        assert_eq!(parsed.tokens, Some(286));
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
        assert_eq!(
            parsed.tokens,
            Some(1300),
            "thinking-indicator above the 10-line window must be recovered \
             by the whole-pane fallback scan"
        );
        assert!(
            saw_bar,
            "⏵⏵ icon at bottom must register as status bar"
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
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(2300));
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_down_arrow() {
        // Some thinking lines use ↓ instead of ↑.
        let input = "\u{25cf} Zigzagging\u{2026} (37s \u{00b7} \u{2193} 1.3k tokens \u{00b7} thought for 13s)";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(1300));
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_no_suffix() {
        // ↑ N tokens (no suffix) — N is a literal integer.
        let input = "\u{25cf} Newspapering\u{2026} (21s \u{00b7} \u{2191} 286 tokens \u{00b7} thought for 1s)";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(286));
    }

    #[test]
    fn test_parse_status_bar_thinking_indicator_m_suffix() {
        // Defensive: M-suffix support for huge contexts (1.4M tokens).
        let input = "\u{25cf} Cooking\u{2026} (5m \u{00b7} \u{2191} 1.4M tokens)";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(1_400_000));
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
}
