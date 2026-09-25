//! Effective-policy loading for the host-bash MCP server.
//!
//! The policy mirrors the knobs the legacy `mcp-host-bash` bash launcher fed
//! into `cli-mcp-server` (allow-list, timeout, shell-operator gate, path
//! fence) plus the `run_script` fences the auth-shim enforced. It is loaded
//! from the SAME operator config file
//! (`~/.config/claude-container/mcp-host-bash.env`) so switching to this
//! single-process server requires no config migration.
//!
//! Precedence mirrors the old `. "$MCP_HOST_BASH_CONFIG"` sourcing: process
//! environment first, then the `.env` file overlaid on top (so a key set in
//! the file wins over an inherited env var), then built-in defaults fill any
//! key neither source provided. An explicit `ALLOWED_COMMANDS` in the file
//! still wins over the `CW_PROFILE`-derived default, exactly as before.
//!
//! `MAX_COMMAND_LENGTH` and `MAX_SCRIPT_LENGTH` are two SEPARATE knobs
//! (`run_command` vs `run_script` respectively) — see the doc comments on
//! [`Policy::max_command_length`] and [`Policy::max_script_length`] for why
//! they must not be the same cap.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Read-y / observation / standard-dev-tool floor — the conservative default
/// allow-list, byte-for-byte the launcher's `DEFAULT_ALLOWED_COMMANDS`.
const DEFAULT_ALLOWED_COMMANDS: &str = "ls,cat,pwd,git,gh,head,tail,grep,find,echo,jq,yq,make,awk,sed,cut,tr,wc,xargs,which,printenv,env,sort,uniq,diff,file,stat,base64,uname,hostname,date,basename,dirname,node,npm,yarn,python,python3,pip,ping,host,dig,nslookup,envchain,jenkins-builds,devbar";

/// `CW_PROFILE=corp-dev-trusted` adds host-scheduling / file-mutation /
/// outbound / container-management binaries on top of the default floor.
const TRUSTED_EXTRAS: &str = "sw_vers,lsb_release,crontab,launchctl,systemctl,schtasks,powershell,pwsh,mkdir,tee,chmod,cp,mv,rm,curl,wget,scp,openssl,ssh-keygen,docker,docker-compose,hostjob";

const DEFAULT_ALLOWED_DIR: &str = "/";
const DEFAULT_ALLOWED_FLAGS: &str = "all";
const DEFAULT_COMMAND_TIMEOUT: u64 = 30;
/// `run_command`'s string is handed to `bash -c` (or exec'd directly with no
/// shell); the OS argv/env ceiling is generous (~128KiB+ per arg on both
/// macOS and Linux), so this floor is a sanity cap, not a real constraint.
const DEFAULT_MAX_COMMAND_LENGTH: usize = 131_072; // 128 KiB
/// `run_script`'s body is fed to the interpreter on STDIN
/// ([`crate::exec::run_with_timeout`]) — there is no OS argv ceiling to
/// respect, so this default is generous headroom, not a meaningful limit.
/// It exists only as a sanity backstop against an accidental multi-GB paste.
const DEFAULT_MAX_SCRIPT_LENGTH: usize = 4 * 1024 * 1024; // 4 MiB

/// Resolved, immutable policy shared across request handlers.
#[derive(Debug, Clone)]
pub struct Policy {
    pub allow_all_commands: bool,
    pub allowed_commands: BTreeSet<String>,
    /// Whether flag filtering is disabled (`ALLOWED_FLAGS=all`). Flag-level
    /// filtering is reported by `show_security_rules` but not enforced here —
    /// the command-name allow-list and the shell-operator gate are the floor
    /// (matching every profile shipped, all of which set `ALLOWED_FLAGS=all`).
    pub allowed_flags_all: bool,
    pub allowed_flags: BTreeSet<String>,
    /// Working directory for spawned commands; also the path fence root when
    /// not `/`. Default `/` disables the fence.
    pub allowed_dir: String,
    pub command_timeout: u64,
    /// Max length (bytes) of a `run_command` command STRING. This string is
    /// either handed to `bash -c` or tokenized and exec'd directly — either
    /// way it ultimately becomes process argv, which the OS bounds (but at a
    /// much higher ceiling than the historical 8192 default). Kept separate
    /// from [`Self::max_script_length`]: run_command's argv path is not the
    /// same delivery mechanism as run_script's stdin path, so one knob
    /// shouldn't govern both.
    pub max_command_length: usize,
    /// Max length (bytes) of a `run_script` script BODY. The body is fed to
    /// the interpreter on stdin (see `run_with_timeout` in exec.rs), never
    /// tokenized and never part of argv, so it has no OS-imposed ceiling
    /// analogous to `max_command_length`'s. Defaults far higher than
    /// `max_command_length` for that reason — capping it at the same value
    /// as run_command defeated run_script's entire purpose (forcing chunked
    /// multi-call workarounds for scripts an interpreter would happily read
    /// from stdin in one shot).
    pub max_script_length: usize,
    pub allow_shell_operators: bool,
    /// The resolved `CW_PROFILE` name, for reporting.
    pub profile: String,
    /// Non-fatal warning (e.g. unknown `CW_PROFILE`) surfaced in the banner.
    pub profile_warning: Option<String>,
}

/// Everything needed to stand up the listener + auth, resolved from config.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_host: String,
    pub port: u16,
    pub bearer: Option<String>,
    pub config_path: PathBuf,
    pub config_present: bool,
    pub policy: Policy,
}

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".config/claude-container/mcp-host-bash.env")
}

/// Default path for the operator's GENERIC host-exec env-injection config.
/// Overridable via the `CW_HOST_EXEC_ENV` env var. May be a single
/// `KEY=VALUE` env-file OR a directory of such files.
fn default_host_exec_env_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".config/claude-container/host-exec-env")
}

/// Resolve the configured host-exec env path: `CW_HOST_EXEC_ENV` if set and
/// non-empty, else [`default_host_exec_env_path`].
pub fn host_exec_env_path() -> PathBuf {
    match std::env::var("CW_HOST_EXEC_ENV") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => default_host_exec_env_path(),
    }
}

/// Load operator-configured environment variables to inject into EVERY shell
/// this server spawns (`run_command` + `run_script`, both paths).
///
/// GENERIC BY DESIGN: cw defines no specific keys — whatever `KEY=VALUE` lines
/// the operator drops into the configured file/dir are returned verbatim, so
/// adding or changing an injected var is pure LOCAL CONFIG (no cw code change,
/// no rebuild — the path is read at spawn time, see [`crate::exec`]).
///
/// Best-effort: a missing path, unreadable file, or parse hiccup yields an
/// empty vec and NEVER errors — env injection must not fail or slow a tool
/// call. The path (`CW_HOST_EXEC_ENV`, default
/// `~/.config/claude-container/host-exec-env`) may be a single env-file or a
/// directory of them, merged in lexical filename order (later files win),
/// mirroring the `docker-compose.override` private-config pattern.
pub fn load_host_exec_env() -> Vec<(String, String)> {
    load_host_exec_env_from(&host_exec_env_path())
}

fn load_host_exec_env_from(path: &Path) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if meta.is_file() {
        if let Ok(text) = std::fs::read_to_string(path) {
            merged.extend(parse_env_file(&text));
        }
    } else if meta.is_dir() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for entry in entries {
            if entry.is_file() {
                if let Ok(text) = std::fs::read_to_string(&entry) {
                    merged.extend(parse_env_file(&text));
                }
            }
        }
    }
    merged.into_iter().collect()
}

/// Parse a bash-`KEY=VALUE` config file leniently. Handles optional `export `,
/// surrounding single/double quotes, and skips blanks + `#` comment lines. We
/// do NOT attempt full bash semantics (no `$VAR` expansion, no command subst)
/// — the file is a flat KEY=VALUE policy file by convention.
fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let mut val = val.trim();
        // Strip one layer of matching surrounding quotes.
        if val.len() >= 2
            && ((val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\'')))
        {
            val = &val[1..val.len() - 1];
        }
        out.insert(key.to_string(), val.to_string());
    }
    out
}

fn to_set(csv: &str) -> BTreeSet<String> {
    csv.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

impl ServerConfig {
    /// Load config from the process environment overlaid with the `.env` file
    /// (file wins), applying defaults for anything unset. `port_override` (from
    /// `--port`) wins over any env-provided port.
    pub fn load(config_path: PathBuf, port_override: Option<u16>) -> Self {
        // Base map = process env; overlay the file so file keys win (mirrors
        // `. "$MCP_HOST_BASH_CONFIG"`).
        let mut map: HashMap<String, String> = std::env::vars().collect();
        let config_present = config_path.is_file();
        if config_present {
            if let Ok(text) = std::fs::read_to_string(&config_path) {
                for (k, v) in parse_env_file(&text) {
                    map.insert(k, v);
                }
            }
        }
        let get = |k: &str| map.get(k).map(|s| s.as_str()).filter(|s| !s.is_empty());

        // Profile → default allow-list.
        let profile = get("CW_PROFILE").unwrap_or("").to_string();
        let mut profile_warning = None;
        let profile_commands = match profile.as_str() {
            "" | "corp-dev" => DEFAULT_ALLOWED_COMMANDS.to_string(),
            "corp-dev-trusted" => format!("{DEFAULT_ALLOWED_COMMANDS},{TRUSTED_EXTRAS}"),
            other => {
                profile_warning =
                    Some(format!("unknown CW_PROFILE='{other}', falling back to default"));
                DEFAULT_ALLOWED_COMMANDS.to_string()
            }
        };
        // CLAUDE_HOOK_BRIDGE_BINS merges into the profile default (union).
        let profile_commands = match get("CLAUDE_HOOK_BRIDGE_BINS") {
            Some(bins) => format!("{profile_commands},{bins}"),
            None => profile_commands,
        };
        // Explicit ALLOWED_COMMANDS wins over the profile-derived default.
        let allowed_commands_csv = get("ALLOWED_COMMANDS")
            .map(|s| s.to_string())
            .unwrap_or(profile_commands);
        let allow_all_commands = allowed_commands_csv.trim() == "all";

        let allowed_flags_csv = get("ALLOWED_FLAGS").unwrap_or(DEFAULT_ALLOWED_FLAGS);
        let allowed_flags_all = allowed_flags_csv.trim() == "all";

        let allowed_dir = get("ALLOWED_DIR").unwrap_or(DEFAULT_ALLOWED_DIR).to_string();
        let command_timeout = get("COMMAND_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT);
        let max_command_length = get("MAX_COMMAND_LENGTH")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_COMMAND_LENGTH);
        let max_script_length = get("MAX_SCRIPT_LENGTH")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_SCRIPT_LENGTH);
        let allow_shell_operators = get("ALLOW_SHELL_OPERATORS")
            .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
            .unwrap_or(false);

        let bind_host = get("MCP_HOST_BASH_BIND").unwrap_or("127.0.0.1").to_string();
        let port = port_override
            .or_else(|| get("MCP_HOST_BASH_PORT").and_then(|s| s.parse().ok()))
            .unwrap_or(8766);
        let bearer = get("MCP_HOST_BASH_BEARER").map(|s| s.to_string());

        ServerConfig {
            bind_host,
            port,
            bearer,
            config_path,
            config_present,
            policy: Policy {
                allow_all_commands,
                allowed_commands: to_set(&allowed_commands_csv),
                allowed_flags_all,
                allowed_flags: to_set(allowed_flags_csv),
                allowed_dir,
                command_timeout,
                max_command_length,
                max_script_length,
                allow_shell_operators,
                profile: if profile.is_empty() {
                    "corp-dev (default)".to_string()
                } else {
                    profile
                },
                profile_warning,
            },
        }
    }

    pub fn default_config_path() -> PathBuf {
        default_config_path()
    }
}

impl Policy {
    /// Human-readable dump of the effective policy for `show_security_rules`.
    pub fn describe(&self) -> String {
        let cmds = if self.allow_all_commands {
            "all (allow-list disabled)".to_string()
        } else {
            self.allowed_commands
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let flags = if self.allowed_flags_all {
            "all (flag filtering off)".to_string()
        } else {
            format!(
                "{} (reported only; not enforced — command-name + shell-operator gate is the floor)",
                self.allowed_flags.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        // Operator-configured env injection: report the path + how many keys
        // are currently loaded (read live, so this reflects the config as of
        // now). Values are NOT printed — they may be secrets.
        let env_path = host_exec_env_path();
        let env_keys: Vec<String> =
            load_host_exec_env().into_iter().map(|(k, _)| k).collect();
        let host_exec_env = if env_keys.is_empty() {
            format!("{} (none loaded)", env_path.display())
        } else {
            format!("{} ({} keys: {})", env_path.display(), env_keys.len(), env_keys.join(", "))
        };
        format!(
            "host-bash MCP server — effective security policy\n\
             profile:               {}\n\
             allowed_commands:      {}\n\
             allowed_flags:         {}\n\
             allowed_dir (cwd):     {}{}\n\
             command_timeout:       {}s\n\
             max_command_length:    {} (run_command)\n\
             max_script_length:     {} (run_script)\n\
             allow_shell_operators: {}\n\
             host_exec_env:         {}\n\
             \n\
             run_command runs the string via `bash -c` when allow_shell_operators=true;\n\
             otherwise it rejects shell metacharacters and exec's the whitelisted binary\n\
             directly (no shell). run_script feeds the script body to the interpreter on\n\
             STDIN (never tokenized). Both are gated by the command allow-list above.",
            self.profile,
            cmds,
            flags,
            self.allowed_dir,
            if self.allowed_dir == "/" {
                " (path fence off)"
            } else {
                " (path fence on)"
            },
            self.command_timeout,
            self.max_command_length,
            self.max_script_length,
            self.allow_shell_operators,
            host_exec_env,
        )
    }

    /// Is `name` (a bare command basename) permitted by the allow-list?
    pub fn command_allowed(&self, name: &str) -> bool {
        self.allow_all_commands || self.allowed_commands.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_env_with_quotes_and_export() {
        let m = parse_env_file(
            "# comment\nexport ALLOWED_COMMANDS=all\nMCP_HOST_BASH_BEARER=\"sekret\"\nCW_PROFILE='corp-dev-trusted'\n\nBAD LINE\n",
        );
        assert_eq!(m.get("ALLOWED_COMMANDS").unwrap(), "all");
        assert_eq!(m.get("MCP_HOST_BASH_BEARER").unwrap(), "sekret");
        assert_eq!(m.get("CW_PROFILE").unwrap(), "corp-dev-trusted");
        assert!(!m.contains_key("BAD"));
    }

    #[test]
    fn host_exec_env_missing_path_is_empty_noop() {
        let missing = PathBuf::from("/nonexistent/cw-host-exec-env-xyz");
        assert!(load_host_exec_env_from(&missing).is_empty());
    }

    #[test]
    fn host_exec_env_reads_flat_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("env");
        std::fs::write(&f, "# a comment\nFOO=bar\nexport BAZ=\"q u x\"\n").unwrap();
        let got: std::collections::HashMap<_, _> =
            load_host_exec_env_from(&f).into_iter().collect();
        assert_eq!(got.get("FOO").unwrap(), "bar");
        assert_eq!(got.get("BAZ").unwrap(), "q u x");
    }

    #[test]
    fn host_exec_env_merges_dir_later_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("00-base"), "A=1\nB=2\n").unwrap();
        std::fs::write(dir.path().join("10-override"), "B=99\nC=3\n").unwrap();
        let got: std::collections::HashMap<_, _> =
            load_host_exec_env_from(dir.path()).into_iter().collect();
        assert_eq!(got.get("A").unwrap(), "1");
        assert_eq!(got.get("B").unwrap(), "99"); // later file wins
        assert_eq!(got.get("C").unwrap(), "3");
    }

    #[test]
    fn trusted_profile_adds_extras_but_default_does_not() {
        let dflt = to_set(DEFAULT_ALLOWED_COMMANDS);
        assert!(dflt.contains("git"));
        assert!(!dflt.contains("docker"));
        let trusted = to_set(&format!("{DEFAULT_ALLOWED_COMMANDS},{TRUSTED_EXTRAS}"));
        assert!(trusted.contains("docker"));
        assert!(trusted.contains("hostjob"));
    }
}
