//! Policy-driven auto-resolution of harmless tool-permission prompts.
//!
//! Claude Code raises "Do you want to proceed?" for a Bash call it considers
//! dangerous even with bypass-permissions on. One false positive is the `rm`
//! safety check: it reasons about a path it mis-resolved (for example
//! `/.gitignore.tmp` for a relative `../../../.gitignore.tmp` temp file inside
//! an agent's own worktree) and blocks an unattended agent until somebody
//! answers. `permission_prompt_monitor` can only DECLINE such a prompt; this
//! module can answer "Yes" to a narrow, explicitly allow-listed class of them.
//!
//! # Safety model
//!
//! This is a way to APPROVE tool calls, so everything is default-deny:
//!
//! 1. **Feature gate.** `mode` is `off` (default), `dry-run` (log what would
//!    be answered, press nothing) or `enforce`. A kill-switch file in the
//!    state dir, or `CLAUDE_WATCH_AUTORESOLVE_DISABLE=1`, forces `off`
//!    immediately, every cycle.
//! 2. **Strict prompt shape.** Only a Bash-command dialog with a
//!    `Bash command[ from <agent>]` title, a command block, the exact
//!    question `Do you want to proceed?`, exactly two options (`1. Yes`,
//!    `2. No...`) and the cursor on option 1. Anything else is ignored.
//! 3. **Whole-command match.** The displayed command is parsed with a small
//!    conservative shell tokenizer that REJECTS every construct it does not
//!    model (pipes, `$`, backticks, globs, subshells, heredocs, `||`, `&`...).
//!    Every segment must be allowed by a rule; every path (arguments,
//!    redirect targets, `cd` targets) is resolved against the command's own
//!    `cd` chain and must land inside an allowed root, lexically and after
//!    symlink resolution. A command with a relative path and no known working
//!    directory is refused.
//! 4. **Hard deny list**, checked first on the raw text and again on resolved
//!    paths: force pushes, cluster/cloud mutations, sudo, docker, curl, ssh,
//!    host-bash, secrets and credential paths, chmod/chown, and so on.
//! 5. **Stability + loop protection.** The same dialog must be on screen,
//!    byte-identical, across two captures; a dialog is never answered twice
//!    without the screen changing; a per-minute rate limit trips into a
//!    cooldown with a high-severity alert.
//! 6. **Audit.** Every answer (and every dry-run "would answer") is appended
//!    to a JSONL file and emitted as an ambient claude-event.
//! 7. **Only key sent is `Enter`**, on option 1, after a fresh capture shows
//!    the cursor on `1. Yes`. Never a digit.

use crate::tmux;
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub struct AutoResolveConfig {
    /// `off` (default) | `dry-run` | `enforce`.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Path to the rules TOML. Empty = no rules = nothing is ever answered.
    #[serde(default)]
    pub rules_file: String,
    /// Directory for the audit log and kill-switch file. Empty = the
    /// directory holding `[general] state_file`.
    #[serde(default)]
    pub state_dir: String,
    /// Maximum auto-answers in any 60 seconds before the feature trips.
    #[serde(default = "default_max_per_minute")]
    pub max_per_minute: usize,
    /// Seconds the feature stays off after tripping.
    #[serde(default = "default_trip_cooldown")]
    pub trip_cooldown_secs: u64,
    /// Seconds between the two confirming captures before any keystroke.
    #[serde(default = "default_settle")]
    pub settle_secs: u64,
    /// Use the pane's current path as the starting cwd for relative paths.
    /// Off by default: the prompt does not show the working directory, and
    /// the pane's cwd is the main loop's, not necessarily the agent's.
    #[serde(default)]
    pub use_pane_cwd: bool,
}

fn default_mode() -> String {
    "off".to_string()
}
fn default_max_per_minute() -> usize {
    6
}
fn default_trip_cooldown() -> u64 {
    900
}
fn default_settle() -> u64 {
    3
}

impl Default for AutoResolveConfig {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            rules_file: String::new(),
            state_dir: String::new(),
            max_per_minute: default_max_per_minute(),
            trip_cooldown_secs: default_trip_cooldown(),
            settle_secs: default_settle(),
            use_pane_cwd: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    DryRun,
    Enforce,
}

impl AutoResolveConfig {
    /// Unknown strings fail closed to `Off`.
    pub fn parsed_mode(&self) -> Mode {
        match self.mode.trim().to_ascii_lowercase().as_str() {
            "dry-run" | "dry_run" | "dryrun" => Mode::DryRun,
            "enforce" => Mode::Enforce,
            _ => Mode::Off,
        }
    }
}

pub const KILL_SWITCH_FILE: &str = "autoresolve.disable";
pub const AUDIT_FILE: &str = "autoresolve-audit.jsonl";
pub const KILL_SWITCH_ENV: &str = "CLAUDE_WATCH_AUTORESOLVE_DISABLE";

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
struct RulesFile {
    #[serde(default)]
    root: Vec<RootSpec>,
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

#[derive(Debug, Deserialize, Clone)]
struct RootSpec {
    path: String,
    /// Path components required BELOW the root (so `.worktrees` +
    /// `min_depth = 3` means `<repo>/<slug>/<file>` at the shallowest).
    #[serde(default = "one")]
    min_depth: usize,
}

fn one() -> usize {
    1
}

#[derive(Debug, Deserialize, Clone)]
struct RuleSpec {
    id: String,
    #[serde(default)]
    description: String,
    /// Only `1` (Yes) is supported; the loader rejects anything else.
    #[serde(default = "one_u32")]
    answer: u32,
    /// Bare program names this rule covers (`rm`, `cat`, ...).
    programs: Vec<String>,
    /// Flags permitted for these programs, exact match (`-f`, `-p`, `-i`).
    #[serde(default)]
    flags: Vec<String>,
    /// Require the dialog to name a sub-agent (`Bash command from <agent>`).
    #[serde(default = "yes")]
    require_subagent: bool,
}

fn one_u32() -> u32 {
    1
}
fn yes() -> bool {
    true
}

/// Programs the engine knows how to check. A rule can only enable names from
/// this list; there is no way to allow an arbitrary binary from config.
const KNOWN_PROGRAMS: &[&str] = &[
    "rm", "cat", "touch", "mkdir", "echo", "printf", "sed", "true", "ls", "cd",
];

#[derive(Debug, Clone)]
pub struct Root {
    pub path: PathBuf,
    canon: PathBuf,
    min_depth: usize,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub description: String,
    pub programs: Vec<String>,
    pub flags: Vec<String>,
    pub require_subagent: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub roots: Vec<Root>,
    pub rules: Vec<Rule>,
}

fn expand_home(p: &str) -> Result<PathBuf, String> {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").map_err(|_| "HOME unset".to_string())?;
        return Ok(PathBuf::from(home).join(rest));
    }
    if p.starts_with('/') {
        return Ok(PathBuf::from(p));
    }
    Err(format!("root {:?} must be absolute or start with ~/", p))
}

impl Policy {
    pub fn from_toml(text: &str) -> Result<Policy, String> {
        let f: RulesFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut roots = Vec::new();
        for r in f.root {
            let path = normalize(&expand_home(&r.path)?);
            // A root this shallow would make "inside a root" meaningless.
            if path.components().count() < 2 || r.min_depth == 0 {
                return Err(format!("root {:?} is too broad", r.path));
            }
            let canon = canonicalize_lenient(&path);
            roots.push(Root {
                path,
                canon,
                min_depth: r.min_depth,
            });
        }
        let mut rules = Vec::new();
        for r in f.rule {
            if r.answer != 1 {
                return Err(format!("rule {}: only answer = 1 (Yes) is supported", r.id));
            }
            for p in &r.programs {
                if !KNOWN_PROGRAMS.contains(&p.as_str()) {
                    return Err(format!("rule {}: unknown program {:?}", r.id, p));
                }
            }
            for fl in &r.flags {
                if matches!(
                    fl.as_str(),
                    "-r" | "-R" | "-rf" | "-fr" | "-d" | "--recursive" | "--no-preserve-root"
                ) {
                    return Err(format!("rule {}: flag {:?} is never allowed", r.id, fl));
                }
            }
            rules.push(Rule {
                id: r.id,
                description: r.description,
                programs: r.programs,
                flags: r.flags,
                require_subagent: r.require_subagent,
            });
        }
        Ok(Policy { roots, rules })
    }

    pub fn load(path: &str) -> Result<Policy, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))?;
        Policy::from_toml(&text)
    }

    fn rule_for(&self, prog: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|r| r.programs.iter().any(|p| p == prog))
    }

    /// Is `p` (already absolute and lexically normalized) inside a root, both
    /// as written and after resolving symlinks?
    fn contains(&self, p: &Path) -> bool {
        let canon = canonicalize_lenient(p);
        self.roots.iter().any(|r| {
            let depth_ok = |path: &Path, base: &Path| {
                path.strip_prefix(base)
                    .map(|rest| rest.components().count() >= r.min_depth)
                    .unwrap_or(false)
            };
            depth_ok(p, &r.path) && depth_ok(&canon, &r.canon)
        })
    }
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

/// Lexical normalization: collapse `.`/`..`/`//`. `..` at the root stays at
/// the root, as a real resolver does.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                if out.parent().is_some() {
                    out.pop();
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// Resolve symlinks in the longest existing prefix, append the rest.
fn canonicalize_lenient(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if std::fs::symlink_metadata(&existing).is_ok() {
            if let Ok(c) = std::fs::canonicalize(&existing) {
                let mut out = c;
                for t in tail.iter().rev() {
                    out.push(t);
                }
                return normalize(&out);
            }
        }
        match (
            existing.file_name().map(|s| s.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => return normalize(p),
        }
    }
}

/// Resolve a path word against `cwd`. `None` when it is relative and no
/// working directory is known.
pub fn resolve_path(cwd: Option<&Path>, raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        Some(normalize(p))
    } else {
        cwd.map(|c| normalize(&c.join(p)))
    }
}

fn is_secret_path(p: &Path) -> bool {
    let comps: Vec<String> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    for (i, c) in comps.iter().enumerate() {
        if matches!(
            c.as_str(),
            ".ssh"
                | ".aws"
                | ".gnupg"
                | ".kube"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".docker"
                | "keychains"
                | "library"
        ) {
            return true;
        }
        if c == ".config" && comps.get(i + 1).map(|n| n == "gh").unwrap_or(false) {
            return true;
        }
    }
    let name = comps.last().cloned().unwrap_or_default();
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.contains("credential")
        || name.contains("secret")
        || name.contains("token")
        || name.contains("keychain")
}

// ---------------------------------------------------------------------------
// Hard deny list
// ---------------------------------------------------------------------------

/// Substrings that make a command un-auto-resolvable no matter what else
/// matches. Defense in depth: the program allow-list already excludes nearly
/// all of these, but this gives a precise audit reason and survives a future
/// widening of the allow-list.
const HARD_DENY: &[(&str, &str)] = &[
    ("sudo", "sudo"),
    ("doas", "sudo"),
    ("docker", "docker"),
    ("podman", "docker"),
    ("kubectl", "cluster"),
    ("helm", "cluster"),
    ("terraform", "cluster"),
    ("tofu", "cluster"),
    ("aws ", "cloud"),
    ("gcloud", "cloud"),
    ("az ", "cloud"),
    ("curl", "network"),
    ("wget", "network"),
    ("ssh", "network"),
    ("scp", "network"),
    ("rsync", "network"),
    ("nc ", "network"),
    ("host-bash", "host"),
    ("hostjob", "host"),
    ("osascript", "host"),
    ("launchctl", "host"),
    ("chmod", "perms"),
    ("chown", "perms"),
    ("chgrp", "perms"),
    ("git push", "git-push"),
    ("git reset", "git-history"),
    ("git rebase", "git-history"),
    ("git filter", "git-history"),
    ("--force", "force"),
    ("--no-preserve-root", "force"),
    ("eval", "eval"),
    ("base64", "obfuscation"),
    ("xargs", "indirection"),
    ("find ", "indirection"),
    ("| sh", "pipe-to-shell"),
    ("|sh", "pipe-to-shell"),
    ("| bash", "pipe-to-shell"),
    ("|bash", "pipe-to-shell"),
    (".env", "secret"),
    (".pem", "secret"),
    (".netrc", "secret"),
    (".ssh", "secret"),
    (".aws", "secret"),
    (".gnupg", "secret"),
    (".kube", "secret"),
    ("config/gh", "secret"),
    ("id_rsa", "secret"),
    ("id_ed25519", "secret"),
    ("keychain", "secret"),
    ("credential", "secret"),
    ("secret", "secret"),
    ("token", "secret"),
    ("password", "secret"),
    ("/etc/", "system"),
    ("/usr/", "system"),
    ("/var/", "system"),
    ("/system", "system"),
    ("/library", "system"),
];

pub fn hard_deny_class(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    HARD_DENY
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, class)| *class)
}

// ---------------------------------------------------------------------------
// Shell tokenizer (deliberately tiny; rejects what it does not model)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub op: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Segment {
    pub words: Vec<String>,
    pub redirects: Vec<Redirect>,
}

fn read_word(chars: &[char], mut i: usize) -> Result<(String, usize), String> {
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' => break,
            '\'' => {
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err("unterminated single quote".into()),
                        Some('\'') => break,
                        Some(&ch) => out.push(ch),
                    }
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err("unterminated double quote".into()),
                        Some('"') => break,
                        Some('$') | Some('`') | Some('\\') | Some('!') => {
                            return Err("expansion or escape inside double quotes".into())
                        }
                        Some(&ch) => out.push(ch),
                    }
                    i += 1;
                }
                i += 1;
            }
            '$' | '`' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | ']' | '!' | '#' | '\\' | '~' => {
                return Err(format!("unsupported shell syntax {:?}", c))
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok((out, i))
}

pub fn tokenize(cmd: &str) -> Result<Vec<Segment>, String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut segs: Vec<Segment> = Vec::new();
    let mut cur = Segment::default();
    let mut i = 0;
    let end_seg = |segs: &mut Vec<Segment>, cur: &mut Segment| {
        if !cur.words.is_empty() || !cur.redirects.is_empty() {
            segs.push(std::mem::take(cur));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' || c == '\t' {
            i += 1;
            continue;
        }
        if c == '\n' || c == ';' {
            end_seg(&mut segs, &mut cur);
            i += 1;
            continue;
        }
        if c == '&' {
            if chars.get(i + 1) == Some(&'&') {
                end_seg(&mut segs, &mut cur);
                i += 2;
                continue;
            }
            return Err("background/`&` is not supported".into());
        }
        if c == '|' {
            return Err("pipes and `||` are not supported".into());
        }
        // Optional fd prefix for a redirect: `2>`.
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        let fd_prefix = j > i && matches!(chars.get(j), Some('<') | Some('>'));
        if fd_prefix {
            i = j;
        }
        let c = chars[i];
        if c == '<' || c == '>' {
            let mut op = String::new();
            op.push(c);
            i += 1;
            if c == '>' && chars.get(i) == Some(&'>') {
                op.push('>');
                i += 1;
            }
            if chars.get(i) == Some(&'<') || chars.get(i) == Some(&'>') {
                return Err("heredoc/here-string/`<>` is not supported".into());
            }
            if chars.get(i) == Some(&'&') {
                // fd duplication: only `>&N`.
                let d = chars.get(i + 1).copied();
                if c == '>' && op == ">" && d.map(|x| x.is_ascii_digit()).unwrap_or(false) {
                    cur.redirects.push(Redirect {
                        op: ">&".into(),
                        target: d.unwrap().to_string(),
                    });
                    i += 2;
                    continue;
                }
                return Err("unsupported fd redirect".into());
            }
            while chars
                .get(i)
                .map(|x| *x == ' ' || *x == '\t')
                .unwrap_or(false)
            {
                i += 1;
            }
            let (target, ni) = read_word(&chars, i)?;
            if target.is_empty() {
                return Err("redirect without target".into());
            }
            cur.redirects.push(Redirect { op, target });
            i = ni;
            continue;
        }
        let (w, ni) = read_word(&chars, i)?;
        if ni == i {
            return Err("unparseable input".into());
        }
        cur.words.push(w);
        i = ni;
    }
    end_seg(&mut segs, &mut cur);
    if segs.is_empty() {
        return Err("empty command".into());
    }
    Ok(segs)
}

// ---------------------------------------------------------------------------
// Command evaluation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow {
        rule_ids: Vec<String>,
        resolved: Vec<PathBuf>,
    },
    Deny {
        reason: String,
    },
}

fn deny<T>(reason: impl Into<String>) -> Result<T, String> {
    Err(reason.into())
}

/// Evaluate a parsed prompt's command against the policy.
pub fn evaluate_prompt(policy: &Policy, prompt: &BashPrompt, start_cwd: Option<&Path>) -> Decision {
    let mut merged: Option<(Vec<String>, Vec<PathBuf>)> = None;
    for v in &prompt.variants {
        match evaluate_command(policy, v, prompt.agent.as_deref(), start_cwd) {
            Decision::Deny { reason } => return Decision::Deny { reason },
            Decision::Allow { rule_ids, resolved } => {
                let m = merged.get_or_insert_with(|| (Vec::new(), Vec::new()));
                for r in rule_ids {
                    if !m.0.contains(&r) {
                        m.0.push(r);
                    }
                }
                for p in resolved {
                    if !m.1.contains(&p) {
                        m.1.push(p);
                    }
                }
            }
        }
    }
    match merged {
        Some((rule_ids, resolved)) => Decision::Allow { rule_ids, resolved },
        None => Decision::Deny {
            reason: "no command".into(),
        },
    }
}

pub fn evaluate_command(
    policy: &Policy,
    command: &str,
    agent: Option<&str>,
    start_cwd: Option<&Path>,
) -> Decision {
    match evaluate_inner(policy, command, agent, start_cwd) {
        Ok((rule_ids, resolved)) => Decision::Allow { rule_ids, resolved },
        Err(reason) => Decision::Deny { reason },
    }
}

fn evaluate_inner(
    policy: &Policy,
    command: &str,
    agent: Option<&str>,
    start_cwd: Option<&Path>,
) -> Result<(Vec<String>, Vec<PathBuf>), String> {
    if policy.rules.is_empty() || policy.roots.is_empty() {
        return deny("no rules or no roots configured");
    }
    if let Some(class) = hard_deny_class(command) {
        return deny(format!("hard-deny:{}", class));
    }
    let segs = tokenize(command).map_err(|e| format!("unparseable:{}", e))?;
    let mut cwd: Option<PathBuf> = start_cwd.map(normalize);
    let mut rule_ids: Vec<String> = Vec::new();
    let mut resolved: Vec<PathBuf> = Vec::new();

    let check_path = |p: &Path| -> Result<(), String> {
        if is_secret_path(p) {
            return deny(format!("hard-deny:secret-path:{}", p.display()));
        }
        if !policy.contains(p) {
            return deny(format!("outside-roots:{}", p.display()));
        }
        Ok(())
    };

    for seg in &segs {
        let Some(prog) = seg.words.first() else {
            return deny("redirect without command");
        };
        if prog.contains('/') {
            return deny("program must be a bare name");
        }
        // Assignments like FOO=bar cmd would put an env var in front.
        if prog.contains('=') {
            return deny("env assignment prefix");
        }
        let args = &seg.words[1..];

        if prog == "cd" {
            if !seg.redirects.is_empty() || args.len() != 1 || args[0].starts_with('-') {
                return deny("unsupported cd form");
            }
            let Some(rule) = policy.rule_for("cd") else {
                return deny("no rule allows cd");
            };
            let Some(p) = resolve_path(cwd.as_deref(), &args[0]) else {
                return deny("relative cd with unknown working directory");
            };
            check_path(&p)?;
            if !rule_ids.contains(&rule.id) {
                rule_ids.push(rule.id.clone());
            }
            if rule.require_subagent && agent.is_none() {
                return deny("rule requires a sub-agent dialog");
            }
            cwd = Some(p);
            continue;
        }

        let Some(rule) = policy.rule_for(prog) else {
            return deny(format!("program not allowed: {}", prog));
        };
        if rule.require_subagent && agent.is_none() {
            return deny("rule requires a sub-agent dialog");
        }
        if !rule_ids.contains(&rule.id) {
            rule_ids.push(rule.id.clone());
        }

        // Flags, then path/literal args.
        let mut paths: Vec<&String> = Vec::new();
        let mut literals_ok = false;
        let mut sed_script_seen = false;
        let mut sed_inplace = false;
        let mut after_dd = false;
        for a in args {
            if !after_dd && a == "--" {
                after_dd = true;
                continue;
            }
            if !after_dd && a.starts_with('-') && a.len() > 1 {
                if prog == "sed"
                    && (a == "-i" || (a.starts_with("-i") && a.len() <= 8 && !a.contains(' ')))
                {
                    // `-i` or `-i.bak`: in-place edit.
                    if !rule.flags.iter().any(|f| f == "-i") {
                        return deny("flag -i not allowed by rule");
                    }
                    sed_inplace = true;
                    continue;
                }
                if prog == "echo" || prog == "printf" {
                    // literal text that happens to start with '-'
                    literals_ok = true;
                    continue;
                }
                if !rule.flags.iter().any(|f| f == a) {
                    return deny(format!("flag not allowed: {}", a));
                }
                continue;
            }
            match prog.as_str() {
                "echo" | "printf" | "true" => {
                    literals_ok = true;
                }
                "sed" => {
                    if !sed_script_seen {
                        sed_script_seen = true;
                        if !sed_script_ok(a) {
                            return deny("sed script not a plain s///");
                        }
                    } else {
                        paths.push(a);
                    }
                }
                _ => paths.push(a),
            }
        }
        let _ = literals_ok;
        match prog.as_str() {
            "rm" | "touch" | "mkdir" | "cat" | "ls" => {
                if paths.is_empty() && prog != "cat" && prog != "ls" {
                    return deny("no target paths");
                }
            }
            "sed" => {
                if !sed_inplace || !sed_script_seen || paths.is_empty() {
                    return deny("sed only allowed as in-place edit of named files");
                }
            }
            _ => {}
        }
        if prog == "rm" && paths.len() > 8 {
            return deny("too many rm targets");
        }
        if paths.is_empty() && matches!(prog.as_str(), "cat" | "ls") && seg.redirects.is_empty() {
            // `cat`/`ls` with no path would read stdin / list the cwd.
            return deny("no target paths");
        }
        for raw in paths {
            if raw.chars().any(|c| matches!(c, '*' | '?' | '[' | ']')) {
                return deny("glob in path");
            }
            let Some(p) = resolve_path(cwd.as_deref(), raw) else {
                return deny(format!(
                    "unresolvable relative path {:?}: no known cwd",
                    raw
                ));
            };
            check_path(&p)?;
            if prog == "rm" {
                // rm of a directory (or a symlink to one) is never allowed.
                if let Ok(md) = std::fs::metadata(&p) {
                    if md.is_dir() {
                        return deny(format!("rm target is a directory: {}", p.display()));
                    }
                }
                if let Ok(md) = std::fs::symlink_metadata(&p) {
                    if md.is_dir() {
                        return deny(format!("rm target is a directory: {}", p.display()));
                    }
                }
            }
            resolved.push(p);
        }
        for r in &seg.redirects {
            if !matches!(prog.as_str(), "cat" | "echo" | "printf" | "true") {
                return deny("redirects only allowed on cat/echo/printf/true");
            }
            match (r.op.as_str(), r.target.as_str()) {
                (">&", "1") | (">&", "2") => {}
                ("<", "/dev/null") | (">", "/dev/null") | (">>", "/dev/null") => {}
                (">", t) | (">>", t) => {
                    let Some(p) = resolve_path(cwd.as_deref(), t) else {
                        return deny(format!(
                            "unresolvable redirect target {:?}: no known cwd",
                            t
                        ));
                    };
                    check_path(&p)?;
                    resolved.push(p);
                }
                _ => return deny(format!("redirect not allowed: {}{}", r.op, r.target)),
            }
        }
    }
    Ok((rule_ids, resolved))
}

/// Only `s<d>old<d>new<d>[g]` with `/` or `|` as delimiter and no escapes,
/// so no `e`/`w`/`r` command can sneak in.
fn sed_script_ok(s: &str) -> bool {
    let mut it = s.chars();
    if it.next() != Some('s') {
        return false;
    }
    let Some(d) = it.next() else { return false };
    if d != '/' && d != '|' {
        return false;
    }
    let rest: String = it.collect();
    let parts: Vec<&str> = rest.split(d).collect();
    if parts.len() != 3 {
        return false;
    }
    if !(parts[2].is_empty() || parts[2] == "g") {
        return false;
    }
    !s.contains('\\') && !s.contains('\n') && !s.contains(';')
}

// ---------------------------------------------------------------------------
// Prompt parsing (strict shape)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashPrompt {
    pub agent: Option<String>,
    /// Command text as displayed (wrapped lines joined with a space).
    pub command: String,
    /// Every reading of the command when the pane wrapped it: wrapped lines
    /// may have been split at a space (dropped by the wrapper) or mid-word, so
    /// each wrap point is tried both ways. ALL variants must be allowed.
    pub variants: Vec<String>,
    /// The `Dangerous rm operation ...` line, if the dialog carried one.
    pub warning: Option<String>,
    pub signature: u64,
}

fn strip_chrome(line: &str) -> &str {
    line.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '\u{2502}'
                    | '\u{2503}'
                    | '|'
                    | '\u{250c}'
                    | '\u{2510}'
                    | '\u{2514}'
                    | '\u{2518}'
                    | '\u{256d}'
                    | '\u{256e}'
                    | '\u{256f}'
                    | '\u{2570}'
                    | '\u{2500}'
                    | '\u{2501}'
                    | '\u{2550}'
            )
    })
}

fn option_row(line: &str) -> Option<(u32, String, bool)> {
    let mut rest = strip_chrome(line);
    let mut cursor = false;
    if let Some(r) = rest.strip_prefix('\u{276f}') {
        cursor = true;
        rest = r.trim_start();
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let label = rest[digits.len()..].strip_prefix('.')?.trim();
    Some((digits.parse().ok()?, label.to_ascii_lowercase(), cursor))
}

/// Warnings the Bash safety check prints that this feature knows how to read.
const ACCEPTED_WARNING_PREFIXES: &[&str] = &["dangerous rm operation on critical path:"];

fn looks_like_description(line: &str) -> bool {
    let first = line.chars().next();
    first.map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        && line.split_whitespace().count() <= 12
        && line
            .chars()
            .all(|c| c.is_ascii_alphabetic() || matches!(c, ' ' | ',' | '\'' | '-'))
}

pub fn prompt_signature(frame_tail: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Cursor-free, chrome-free text.
    for l in frame_tail.lines() {
        let s = strip_chrome(l);
        let s = s.strip_prefix('\u{276f}').map(str::trim_start).unwrap_or(s);
        s.hash(&mut h);
    }
    h.finish()
}

/// Parse a captured pane into the exact prompt shape, or say why not.
/// A command line that reaches the box's full width is treated as wrapped
/// onto the next line; see `BashPrompt::variants`.
pub fn parse_bash_prompt(frame: &str) -> Result<BashPrompt, String> {
    let lines: Vec<&str> = frame.lines().collect();
    let start = lines.len().saturating_sub(40);
    let tail = &lines[start..];

    let q = tail
        .iter()
        .rposition(|l| strip_chrome(l) == "Do you want to proceed?")
        .ok_or("no 'Do you want to proceed?' line")?;

    // Options: rows directly below the question (blank/border rows skipped).
    let mut opts: Vec<(u32, String, bool)> = Vec::new();
    for l in &tail[q + 1..] {
        let s = strip_chrome(l);
        if s.is_empty() {
            continue;
        }
        if let Some(o) = option_row(l) {
            opts.push(o);
            continue;
        }
        let lower = s.to_ascii_lowercase();
        if lower.contains("esc to cancel")
            || lower.contains("tab to amend")
            || lower.starts_with("esc")
        {
            continue;
        }
        return Err(format!("unexpected line below options: {:?}", s));
    }
    if opts.len() != 2 || opts[0].0 != 1 || opts[1].0 != 2 {
        return Err("options are not exactly 1 and 2".into());
    }
    if opts[0].1 != "yes" {
        return Err(format!("option 1 is not a plain Yes: {:?}", opts[0].1));
    }
    if !opts[1].1.starts_with("no") {
        return Err("option 2 is not No".into());
    }
    if !opts[0].2 || opts[1].2 {
        return Err("cursor is not on option 1".into());
    }

    // Title: nearest 'Bash command' line above the question, within 25 rows.
    let lo = q.saturating_sub(25);
    let t = (lo..q)
        .rev()
        .find(|&i| {
            strip_chrome(tail[i])
                .to_ascii_lowercase()
                .starts_with("bash command")
        })
        .ok_or("no 'Bash command' title")?;
    let title = strip_chrome(tail[t]);
    let rest = title["bash command".len()..].trim();
    let agent = if rest.is_empty() {
        None
    } else if let Some(a) = rest.strip_prefix("from ") {
        let a = a.trim();
        let a = a.strip_prefix("the ").unwrap_or(a);
        let a = a.strip_suffix(" agent").unwrap_or(a).trim();
        if a.is_empty()
            || !a
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | ':' | '.'))
        {
            return Err("unreadable agent name".into());
        }
        Some(a.to_string())
    } else {
        return Err(format!("unrecognized title suffix: {:?}", rest));
    };

    // Block between title and question.
    let box_width = tail[t..=q]
        .iter()
        .map(|l| l.trim_end().chars().count())
        .max()
        .unwrap_or(0);
    let mut block: Vec<(String, bool)> = Vec::new(); // (text, reaches box width)
    for l in &tail[t + 1..q] {
        let s = strip_chrome(l);
        if s.is_empty() {
            continue;
        }
        let full = box_width > 0 && l.trim_end().chars().count() + 3 >= box_width;
        block.push((s.to_string(), full));
    }
    let mut warning = None;
    if let Some((last, _)) = block.last() {
        if last.to_ascii_lowercase().starts_with("dangerous ") {
            if ACCEPTED_WARNING_PREFIXES
                .iter()
                .any(|p| last.to_ascii_lowercase().starts_with(p))
            {
                warning = block.pop().map(|b| b.0);
            } else {
                return Err(format!("unrecognized warning: {:?}", last));
            }
        }
    }
    // Trailing prose description line (never part of the command), unless the
    // line above it ran to the box edge (then it may be a wrapped tail).
    if block.len() >= 2
        && looks_like_description(&block.last().unwrap().0)
        && !block[block.len() - 2].1
    {
        block.pop();
    }
    if block.is_empty() {
        return Err("empty command block".into());
    }
    // Build the variants: each wrap point joined with a space or directly.
    let wraps: Vec<usize> = (0..block.len() - 1).filter(|&i| block[i].1).collect();
    if wraps.len() > 4 {
        return Err("command wrapped over too many lines".into());
    }
    let mut variants: Vec<String> = Vec::new();
    for mask in 0..(1u32 << wraps.len()) {
        let mut out = String::new();
        for (i, (text, _)) in block.iter().enumerate() {
            out.push_str(text);
            if i + 1 < block.len() {
                match wraps.iter().position(|&w| w == i) {
                    Some(k) if mask & (1 << k) != 0 => {}
                    Some(_) => out.push(' '),
                    None => out.push('\n'),
                }
            }
        }
        if !variants.contains(&out) {
            variants.push(out);
        }
    }
    let command = variants[0].clone();
    let signature = prompt_signature(&tail[t..].join("\n"));
    Ok(BashPrompt {
        agent,
        command,
        variants,
        warning,
        signature,
    })
}

// ---------------------------------------------------------------------------
// Rate limiter
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct RateLimiter {
    stamps: VecDeque<Instant>,
}

impl RateLimiter {
    /// Record an answer at `now`; returns false if this one exceeds `max`
    /// answers in the trailing minute (the caller must NOT answer then).
    pub fn admit(&mut self, now: Instant, max: usize) -> bool {
        while let Some(&f) = self.stamps.front() {
            if now.duration_since(f) > Duration::from_secs(60) {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
        if self.stamps.len() >= max {
            return false;
        }
        self.stamps.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// Runtime driver
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct RuntimeState {
    /// Signature of a candidate seen once, awaiting the confirming capture.
    candidate: Option<(u64, Instant)>,
    last_answered: Option<u64>,
    last_refused: Option<u64>,
    limiter: RateLimiter,
    tripped_until: Option<Instant>,
    trip_alerted: bool,
}

static RUNTIME: Mutex<Option<RuntimeState>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Feature off, killed, or no matching dialog.
    Nothing,
    /// A dialog is on screen and is being considered; do not treat as idle.
    Considering,
    Refused {
        reason: String,
    },
    WouldAnswer {
        rule_ids: Vec<String>,
    },
    Answered {
        rule_ids: Vec<String>,
    },
    AnswerFailed {
        reason: String,
    },
    /// Rate limit tripped; caller should page the operator once.
    Tripped,
}

pub fn kill_switch_active(state_dir: &Path) -> bool {
    if std::env::var(KILL_SWITCH_ENV)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
    {
        return true;
    }
    state_dir.join(KILL_SWITCH_FILE).exists()
}

pub fn resolve_state_dir(cfg: &AutoResolveConfig, state_file: &str) -> PathBuf {
    if !cfg.state_dir.trim().is_empty() {
        return PathBuf::from(cfg.state_dir.trim());
    }
    Path::new(state_file)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Append one audit line. Append-only; errors are swallowed (the audit is
/// best-effort but the keystroke path checks it was attempted first).
fn audit(state_dir: &Path, entry: serde_json::Value) -> bool {
    use std::io::Write;
    let _ = std::fs::create_dir_all(state_dir);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(AUDIT_FILE))
    {
        Ok(mut f) => writeln!(f, "{}", entry).is_ok(),
        Err(_) => false,
    }
}

fn emit_audit_event(kind: &'static str, msg: &str) {
    crate::event_bus::emit(&crate::event_bus::ClaudeWatchAlert {
        alert_type: kind,
        stuck_reason: "auto-resolved permission prompt (audit)",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: crate::event_bus::Severity::Low,
        message: msg,
    });
}

async fn pane_query(pane: &str, fmt: &str) -> Option<String> {
    crate::cmd::run_cmd(&["tmux", "display-message", "-p", "-t", pane, fmt], 5).await
}

/// One daemon cycle. Safe to call every cycle; does nothing unless enabled.
pub async fn step(cfg: &AutoResolveConfig, state_file: &str, pane: &str) -> StepOutcome {
    let mode = cfg.parsed_mode();
    let state_dir = resolve_state_dir(cfg, state_file);
    if mode == Mode::Off || pane.is_empty() || kill_switch_active(&state_dir) {
        return StepOutcome::Nothing;
    }
    let Some(frame) = tmux::capture_pane(pane).await else {
        return StepOutcome::Nothing;
    };
    let prompt = match parse_bash_prompt(&frame) {
        Ok(p) => p,
        Err(_) => {
            // Not our dialog shape (or none at all): forget any candidate.
            if let Some(s) = RUNTIME.lock().unwrap().as_mut() {
                s.candidate = None;
                s.last_answered = None;
                s.last_refused = None;
            }
            return StepOutcome::Nothing;
        }
    };
    let now = Instant::now();

    // Tripped cooldown.
    {
        let mut g = RUNTIME.lock().unwrap();
        let s = g.get_or_insert_with(RuntimeState::default);
        if let Some(until) = s.tripped_until {
            if now < until {
                return StepOutcome::Considering;
            }
            s.tripped_until = None;
            s.trip_alerted = false;
        }
        if s.last_answered == Some(prompt.signature) {
            // Same screen after we answered: never answer twice.
            return StepOutcome::Considering;
        }
        if s.last_refused == Some(prompt.signature) {
            return StepOutcome::Considering;
        }
        match s.candidate {
            Some((sig, first)) if sig == prompt.signature => {
                if now.duration_since(first) < Duration::from_secs(cfg.settle_secs) {
                    return StepOutcome::Considering;
                }
            }
            _ => {
                s.candidate = Some((prompt.signature, now));
                return StepOutcome::Considering;
            }
        }
    }

    // Decide.
    let policy = match Policy::load(&cfg.rules_file) {
        Ok(p) => p,
        Err(e) => {
            return refuse(
                &state_dir,
                pane,
                &prompt,
                &format!("rules unavailable: {}", e),
                mode,
            );
        }
    };
    let cwd: Option<PathBuf> = if cfg.use_pane_cwd {
        pane_query(pane, "#{pane_current_path}")
            .await
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| p.is_absolute())
    } else {
        None
    };
    let (rule_ids, resolved) = match evaluate_prompt(&policy, &prompt, cwd.as_deref()) {
        Decision::Allow { rule_ids, resolved } => (rule_ids, resolved),
        Decision::Deny { reason } => return refuse(&state_dir, pane, &prompt, &reason, mode),
    };

    let entry = |kind: &str, answer: &str| {
        serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": kind,
            "pane": pane,
            "agent": prompt.agent,
            "command": prompt.command,
            "warning": prompt.warning,
            "cwd": cwd.as_ref().map(|c| c.display().to_string()),
            "resolved_paths": resolved.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "rule_ids": rule_ids,
            "answer": answer,
            "mode": if mode == Mode::Enforce { "enforce" } else { "dry-run" },
        })
    };

    if mode == Mode::DryRun {
        audit(&state_dir, entry("would-answer", "1"));
        emit_audit_event(
            "autoresolve-dry-run",
            &format!(
                "autoresolve DRY-RUN: would answer 1 (Yes) via {:?}: {}",
                rule_ids, prompt.command
            ),
        );
        let mut g = RUNTIME.lock().unwrap();
        g.get_or_insert_with(RuntimeState::default).last_refused = Some(prompt.signature);
        return StepOutcome::WouldAnswer { rule_ids };
    }

    // Enforce: rate limit, audit-first, final fresh capture, Enter.
    {
        let mut g = RUNTIME.lock().unwrap();
        let s = g.get_or_insert_with(RuntimeState::default);
        if !s.limiter.admit(now, cfg.max_per_minute) {
            s.tripped_until = Some(now + Duration::from_secs(cfg.trip_cooldown_secs));
            let first = !s.trip_alerted;
            s.trip_alerted = true;
            drop(g);
            audit(&state_dir, entry("tripped", "none"));
            return if first {
                StepOutcome::Tripped
            } else {
                StepOutcome::Considering
            };
        }
    }
    if !audit(&state_dir, entry("answering", "1")) {
        // No audit trail, no keystroke.
        return StepOutcome::AnswerFailed {
            reason: "audit log not writable".into(),
        };
    }
    // Fresh capture must still be the same dialog with the cursor on Yes.
    let again = tmux::capture_pane(pane)
        .await
        .and_then(|f| parse_bash_prompt(&f).ok());
    if again.as_ref().map(|p| p.signature) != Some(prompt.signature) {
        audit(&state_dir, entry("aborted-screen-changed", "none"));
        return StepOutcome::AnswerFailed {
            reason: "screen changed before keystroke".into(),
        };
    }
    tmux::send_keys(pane, &["Enter"]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let cleared = tmux::capture_pane(pane)
        .await
        .and_then(|f| parse_bash_prompt(&f).ok())
        .map(|p| p.signature != prompt.signature)
        .unwrap_or(true);
    {
        let mut g = RUNTIME.lock().unwrap();
        let s = g.get_or_insert_with(RuntimeState::default);
        s.last_answered = Some(prompt.signature);
        s.candidate = None;
    }
    audit(
        &state_dir,
        entry(
            if cleared {
                "answered"
            } else {
                "answered-not-cleared"
            },
            "1",
        ),
    );
    emit_audit_event(
        "autoresolve-answered",
        &format!(
            "autoresolve: answered 1 (Yes) via {:?} for agent {:?}: {}",
            rule_ids, prompt.agent, prompt.command
        ),
    );
    if cleared {
        StepOutcome::Answered { rule_ids }
    } else {
        StepOutcome::AnswerFailed {
            reason: "Enter sent but dialog still on screen".into(),
        }
    }
}

fn refuse(
    state_dir: &Path,
    pane: &str,
    prompt: &BashPrompt,
    reason: &str,
    mode: Mode,
) -> StepOutcome {
    audit(
        state_dir,
        serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": "refused",
            "pane": pane,
            "agent": prompt.agent,
            "command": prompt.command,
            "reason": reason,
            "mode": if mode == Mode::Enforce { "enforce" } else { "dry-run" },
        }),
    );
    let mut g = RUNTIME.lock().unwrap();
    g.get_or_insert_with(RuntimeState::default).last_refused = Some(prompt.signature);
    StepOutcome::Refused {
        reason: reason.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::useless_format)]
mod tests {
    use super::*;

    const RULES: &str = include_str!("../examples/autoresolve-rules.toml");
    const WT: &str = "/home/user/repos/.worktrees/myrepo/fix-thing";

    fn policy() -> Policy {
        // The shipped example uses ~ ; pin HOME-independent roots for tests.
        let text = RULES.replace("~/", "/home/user/");
        Policy::from_toml(&text).expect("example rules parse")
    }

    const SHOT: &str = include_str!("../tests/fixtures/autoresolve_screenshot_rm.txt");
    const NEG_RM_RF: &str = include_str!("../tests/fixtures/autoresolve_neg_rm_rf_dir.txt");
    const NEG_OUTSIDE: &str = include_str!("../tests/fixtures/autoresolve_neg_rm_outside.txt");
    const NEG_PUSH: &str = include_str!("../tests/fixtures/autoresolve_neg_git_push_force.txt");
    const NEG_KUBECTL: &str = include_str!("../tests/fixtures/autoresolve_neg_kubectl.txt");
    const NEG_SECRET: &str = include_str!("../tests/fixtures/autoresolve_neg_secret_path.txt");
    const NEG_PARTIAL: &str = include_str!("../tests/fixtures/autoresolve_neg_partial.txt");
    const NEG_UNKNOWN: &str = include_str!("../tests/fixtures/autoresolve_neg_unknown_prompt.txt");

    fn decide(fx: &str) -> Result<Decision, String> {
        let p = parse_bash_prompt(fx)?;
        Ok(evaluate_prompt(&policy(), &p, None))
    }

    fn denied(fx: &str) -> String {
        match decide(fx) {
            Err(e) => format!("parse:{}", e),
            Ok(Decision::Deny { reason }) => reason,
            Ok(Decision::Allow { .. }) => panic!("must NOT be auto-answered:\n{}", fx),
        }
    }

    #[test]
    fn screenshot_prompt_is_matched_by_the_rm_rule() {
        let p = parse_bash_prompt(SHOT).expect("strict shape parses");
        assert_eq!(p.agent.as_deref(), Some("general-purpose"));
        assert!(p.warning.as_deref().unwrap().contains("/.gitignore.tmp"));
        // The mangled displayed path is NOT used; the command text is.
        assert!(
            p.variants
                .iter()
                .any(|v| v.contains("rm -f ../../../.gitignore.tmp")),
            "{:?}",
            p.variants
        );
        assert!(p.variants.len() > 1, "screenshot command wraps in the box");
        match evaluate_prompt(&policy(), &p, None) {
            Decision::Allow { rule_ids, resolved } => {
                assert!(
                    rule_ids.contains(&"rm-file-in-roots".to_string()),
                    "{:?}",
                    rule_ids
                );
                assert!(resolved.iter().any(|r| r.ends_with(".gitignore.tmp")));
                for r in &resolved {
                    assert!(r.starts_with(WT), "resolved outside worktree: {:?}", r);
                }
            }
            Decision::Deny { reason } => panic!("screenshot must be allowed: {}", reason),
        }
    }

    #[test]
    fn relative_path_resolves_inside_the_worktree() {
        let cwd =
            Path::new("/home/user/repos/.worktrees/myrepo/fix-thing/tools/slack-reader/tests");
        let got = resolve_path(Some(cwd), "../../../.gitignore.tmp").unwrap();
        assert_eq!(
            got,
            Path::new("/home/user/repos/.worktrees/myrepo/fix-thing/.gitignore.tmp")
        );
        assert!(policy().contains(&got));
        // One level shallower the SAME relative path escapes the worktree
        // and must be refused: depth matters, so cwd must be known.
        let shallow = Path::new("/home/user/repos/.worktrees/myrepo/fix-thing/tools/slack-reader");
        let esc = resolve_path(Some(shallow), "../../../.gitignore.tmp").unwrap();
        assert_eq!(
            esc,
            Path::new("/home/user/repos/.worktrees/myrepo/.gitignore.tmp")
        );
        assert!(!policy().contains(&esc));
    }

    #[test]
    fn relative_rm_without_known_cwd_is_refused() {
        let d = evaluate_command(&policy(), "rm -f ../../../x.tmp", Some("a"), None);
        assert!(matches!(d, Decision::Deny { .. }), "{:?}", d);
    }

    #[test]
    fn negative_fixtures_are_never_answered() {
        for (name, fx) in [
            ("rm -rf dir", NEG_RM_RF),
            ("rm outside roots", NEG_OUTSIDE),
            ("git push --force", NEG_PUSH),
            ("kubectl delete", NEG_KUBECTL),
            ("secret path", NEG_SECRET),
        ] {
            let reason = denied(fx);
            eprintln!("{name}: {reason}");
        }
    }

    #[test]
    fn specific_denial_reasons() {
        assert!(
            denied(NEG_PUSH).starts_with("hard-deny:"),
            "{}",
            denied(NEG_PUSH)
        );
        assert!(denied(NEG_KUBECTL).starts_with("hard-deny:cluster"));
        assert!(denied(NEG_SECRET).starts_with("hard-deny:secret"));
        assert!(
            denied(NEG_OUTSIDE).starts_with("outside-roots:"),
            "{}",
            denied(NEG_OUTSIDE)
        );
        assert!(
            denied(NEG_RM_RF).contains("flag not allowed: -rf"),
            "{}",
            denied(NEG_RM_RF)
        );
    }

    #[test]
    fn malformed_and_unknown_screens_do_not_parse() {
        assert!(parse_bash_prompt(NEG_PARTIAL).is_err());
        assert!(parse_bash_prompt(NEG_UNKNOWN).is_err());
        assert!(parse_bash_prompt("").is_err());
    }

    #[test]
    fn shape_requirements() {
        // Cursor on option 2.
        let on2 = SHOT
            .replace("\u{276f} 1. Yes", "  1. Yes")
            .replace("  2. No", "\u{276f} 2. No");
        assert!(parse_bash_prompt(&on2).is_err());
        // Third option ("don't ask again") present.
        let three = SHOT.replace("2. No", "2. Yes, and don't ask again\n  3. No");
        assert!(parse_bash_prompt(&three).is_err());
        // Live prompt below the dialog = it is scrollback.
        let sb = format!("{}\n\u{276f} next thing", SHOT);
        assert!(parse_bash_prompt(&sb).is_err());
        // Different title.
        let t = SHOT.replace("Bash command", "Edit file");
        assert!(parse_bash_prompt(&t).is_err());
        // Different question.
        let q = SHOT.replace("Do you want to proceed?", "Do you want to make this edit?");
        assert!(parse_bash_prompt(&q).is_err());
        // Unrecognized warning kind.
        let w = SHOT.replace(
            "Dangerous rm operation on critical path",
            "Dangerous rm operation on possibly-empty variable path",
        );
        assert!(parse_bash_prompt(&w).is_err());
    }

    #[test]
    fn main_loop_dialog_is_not_answered_by_subagent_rules() {
        let d = evaluate_command(&policy(), &format!("rm -f {}/a.tmp", WT), None, None);
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn tokenizer_rejects_unmodelled_syntax() {
        for bad in [
            "rm -f a | cat",
            "rm -f $(echo a)",
            "rm -f `echo a`",
            "rm -f $HOME/x",
            "rm -f ~/x",
            "rm -f *.tmp",
            "rm -f a || true",
            "rm -f a &",
            "cat <<EOF\nx\nEOF",
            "rm -f a #c",
            "echo \"$X\"",
            "rm -f 'a",
        ] {
            let d = evaluate_command(
                &policy(),
                &bad.replace("a", &format!("{}/a", WT)),
                Some("x"),
                None,
            );
            assert!(matches!(d, Decision::Deny { .. }), "{bad:?} -> {d:?}");
        }
    }

    #[test]
    fn allowed_command_shapes() {
        let ok = |c: &str| {
            let d = evaluate_command(&policy(), c, Some("x"), None);
            assert!(matches!(d, Decision::Allow { .. }), "{c:?} -> {d:?}");
        };
        ok(&format!("rm -f {WT}/a.tmp {WT}/b.tmp"));
        ok(&format!("rm {WT}/a.tmp"));
        ok(&format!("cd {WT}/src && rm -f ../a.tmp"));
        ok(&format!("cat >> {WT}/x.txt </dev/null; rm -f {WT}/x.txt"));
        ok(&format!("sed -i 's/a/b/g' {WT}/t.rs"));
        ok("rm -f /tmp/scratch.json");
    }

    #[test]
    fn disallowed_command_shapes() {
        let no = |c: &str| {
            let d = evaluate_command(&policy(), c, Some("x"), None);
            assert!(matches!(d, Decision::Deny { .. }), "{c:?} -> {d:?}");
        };
        no(&format!("rm -r {WT}/a"));
        no(&format!("rm -R {WT}/a"));
        no(&format!("rm -rf {WT}/a"));
        no(&format!("rm -f {WT}"));
        no(&format!("rm -f {WT}/../../x"));
        no("rm -f /tmp");
        no("rm -f /home/user/repos/.worktrees/myrepo/x");
        no("rm -f /home/user/repos/x");
        no("rm -f /etc/hosts");
        no("cd /etc && rm -f x");
        no(&format!("sed -i 's/a/b/e' {WT}/t.rs"));
        no(&format!("sed -i 'w /tmp/x' {WT}/t.rs"));
        no(&format!("sed 's/a/b/' {WT}/t.rs"));
        no("cat > /etc/x");
        no(&format!("echo hi > {WT}/../../../x"));
        no(&format!("/bin/rm -f {WT}/a"));
        no(&format!("FOO=1 rm -f {WT}/a"));
        no(&format!("rm -f {WT}/.env"));
        no(&format!("rm -f {WT}/id_rsa"));
        no("python -c 'x'");
        no(&format!("git -C {WT} push --force"));
        no(&format!("sudo rm -f {WT}/a"));
    }

    #[test]
    fn rules_loader_refuses_dangerous_config() {
        let bad = |t: &str| assert!(Policy::from_toml(t).is_err(), "{t}");
        bad("[[root]]\npath=\"/\"\n");
        bad("[[root]]\npath=\"/tmp\"\nmin_depth=0\n");
        bad("[[rule]]\nid=\"x\"\nprograms=[\"bash\"]\n");
        bad("[[rule]]\nid=\"x\"\nprograms=[\"rm\"]\nflags=[\"-rf\"]\n");
        bad("[[rule]]\nid=\"x\"\nprograms=[\"rm\"]\nanswer=2\n");
        // No rules / roots -> default deny.
        let empty = Policy::from_toml("").unwrap();
        assert!(matches!(
            evaluate_command(&empty, "rm -f /tmp/a", Some("x"), None),
            Decision::Deny { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused_and_directories_are_not_removed() {
        let td = tempfile::Builder::new()
            .prefix("cwar")
            .tempdir_in("/tmp")
            .unwrap();
        let root = td.path().join("wt");
        let inner = root.join("repo").join("slug");
        std::fs::create_dir_all(&inner).unwrap();
        let outside = td.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, inner.join("link")).unwrap();
        std::fs::create_dir_all(inner.join("adir")).unwrap();
        std::fs::write(inner.join("ok.tmp"), "x").unwrap();
        let toml = format!(
            "[[root]]\npath=\"{}\"\nmin_depth=3\n[[rule]]\nid=\"rm\"\nprograms=[\"rm\"]\nflags=[\"-f\"]\n",
            root.display()
        );
        let p = Policy::from_toml(&toml).unwrap();
        let ev = |c: String| evaluate_command(&p, &c, Some("a"), None);
        let d = ev(format!("rm -f {}/ok.tmp", inner.display()));
        assert!(matches!(d, Decision::Allow { .. }), "{:?}", d);
        assert!(matches!(
            ev(format!("rm -f {}/link/victim", inner.display())),
            Decision::Deny { .. }
        ));
        assert!(matches!(
            ev(format!("rm -f {}/adir", inner.display())),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn rate_limiter_admits_up_to_max_per_minute() {
        let mut r = RateLimiter::default();
        let t0 = Instant::now();
        assert!(r.admit(t0, 2));
        assert!(r.admit(t0, 2));
        assert!(!r.admit(t0, 2));
        assert!(r.admit(t0 + Duration::from_secs(61), 2));
    }

    #[test]
    fn mode_parsing_fails_closed() {
        let m = |s: &str| {
            AutoResolveConfig {
                mode: s.into(),
                ..Default::default()
            }
            .parsed_mode()
        };
        assert_eq!(m("off"), Mode::Off);
        assert_eq!(m(""), Mode::Off);
        assert_eq!(m("true"), Mode::Off);
        assert_eq!(m("yes"), Mode::Off);
        assert_eq!(m("dry-run"), Mode::DryRun);
        assert_eq!(m("enforce"), Mode::Enforce);
        assert_eq!(AutoResolveConfig::default().parsed_mode(), Mode::Off);
    }

    #[test]
    fn kill_switch_file_and_audit_append() {
        let td = tempfile::tempdir().unwrap();
        assert!(!kill_switch_active(td.path()));
        std::fs::write(td.path().join(KILL_SWITCH_FILE), "").unwrap();
        assert!(kill_switch_active(td.path()));
        assert!(audit(td.path(), serde_json::json!({"a": 1})));
        assert!(audit(td.path(), serde_json::json!({"a": 2})));
        let t = std::fs::read_to_string(td.path().join(AUDIT_FILE)).unwrap();
        assert_eq!(t.lines().count(), 2);
    }

    #[test]
    fn signature_ignores_cursor_but_not_command() {
        let a = parse_bash_prompt(SHOT).unwrap().signature;
        let b = parse_bash_prompt(&SHOT.replace("test_reader.py", "other_file.py"))
            .unwrap()
            .signature;
        assert_ne!(a, b);
    }
}
