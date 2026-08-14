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

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

/// Read-y / observation / standard-dev-tool floor — the conservative default
/// allow-list, byte-for-byte the launcher's `DEFAULT_ALLOWED_COMMANDS`.
const DEFAULT_ALLOWED_COMMANDS: &str = "ls,cat,pwd,git,gh,head,tail,grep,find,echo,jq,yq,make,awk,sed,cut,tr,wc,xargs,which,printenv,env,sort,uniq,diff,file,stat,base64,uname,hostname,date,basename,dirname,node,npm,yarn,python,python3,pip,ping,host,dig,nslookup,envchain,jenkins-builds,devbar";

/// `CW_PROFILE=corp-dev-trusted` adds host-scheduling / file-mutation /
/// outbound / container-management binaries on top of the default floor.
const TRUSTED_EXTRAS: &str = "sw_vers,lsb_release,crontab,launchctl,systemctl,schtasks,powershell,pwsh,mkdir,tee,chmod,cp,mv,rm,curl,wget,scp,openssl,ssh-keygen,docker,docker-compose,hostjob";

const DEFAULT_ALLOWED_DIR: &str = "/";
const DEFAULT_ALLOWED_FLAGS: &str = "all";
const DEFAULT_COMMAND_TIMEOUT: u64 = 30;
const DEFAULT_MAX_COMMAND_LENGTH: usize = 8192;

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
    pub max_command_length: usize,
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
        format!(
            "host-bash MCP server — effective security policy\n\
             profile:               {}\n\
             allowed_commands:      {}\n\
             allowed_flags:         {}\n\
             allowed_dir (cwd):     {}{}\n\
             command_timeout:       {}s\n\
             max_command_length:    {}\n\
             allow_shell_operators: {}\n\
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
            self.allow_shell_operators,
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
    fn trusted_profile_adds_extras_but_default_does_not() {
        let dflt = to_set(DEFAULT_ALLOWED_COMMANDS);
        assert!(dflt.contains("git"));
        assert!(!dflt.contains("docker"));
        let trusted = to_set(&format!("{DEFAULT_ALLOWED_COMMANDS},{TRUSTED_EXTRAS}"));
        assert!(trusted.contains("docker"));
        assert!(trusted.contains("hostjob"));
    }
}
