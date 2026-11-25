//! Mod side of `[autoresolve]`: the decider behind `mods/cw-autoresolve`.
//!
//! The Claude Code mod hooks `tool.check` for Bash. When the engine's own
//! verdict is `ask` (the "Dangerous rm operation ..." class of dialog that no
//! permission rule can silence) it pipes the command to
//! `claude-watch autoresolve-mod decide`, which answers with the SAME rules
//! file and semantics as the tmux path (`crate::autoresolve`):
//!
//! * allow-listed command -> `allow` (the dialog is never drawn),
//! * every other command -> `deny` (the model reads the reason as the tool
//!   error and rewrites the command),
//! * `abstain` when the feature is off, killed, tripped, in dry-run, set to
//!   `prefer_mod = false`, or anything unexpected: the engine's verdict
//!   stands and the tmux path remains the fallback.
//!
//! Audit, kill switch, rate limit and trip cooldown all live in files under
//! the autoresolve state directory (the decider is a short-lived process, so
//! nothing is kept in memory).

use crate::autoresolve::{
    self, audit, decide_command_text, emit_audit_event as raw_audit_event,
    emit_denial_event as raw_denial_event, kill_switch_active, mode_str, refusal_note,
    resolve_state_dir, AutoResolveConfig, Mode, Policy, Verdict,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Commands longer than this are never evaluated (always No): the rule
/// tokenizer is for short chained commands, not scripts.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModRequest {
    pub command: String,
    /// The sub-agent's id when the call was made inside an agent loop.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// The engine's own reason line for the ask, if it gave one.
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "answer", rename_all = "lowercase")]
pub enum ModAnswer {
    Allow {
        rule_ids: Vec<String>,
    },
    Deny {
        reason: String,
        /// Text the mod hands the model as the tool error.
        note: String,
    },
    Abstain {
        reason: String,
    },
}

// Unit tests must not write into the real claude-events queue.
fn emit_audit_event(kind: &'static str, msg: &str) {
    if !cfg!(test) {
        raw_audit_event(kind, msg);
    }
}

fn emit_denial_event(
    agent: Option<&str>,
    agent_id: Option<&str>,
    notified: bool,
    dialog_reason: Option<&str>,
    reason: &str,
    command: &str,
) {
    if !cfg!(test) {
        raw_denial_event(agent, agent_id, notified, dialog_reason, reason, command);
    }
}

fn abstain(reason: impl Into<String>) -> ModAnswer {
    ModAnswer::Abstain {
        reason: reason.into(),
    }
}

/// Path of the mod heartbeat file.
pub fn heartbeat_path(cfg: &AutoResolveConfig, state_file: &str) -> PathBuf {
    if !cfg.mod_heartbeat_file.trim().is_empty() {
        return PathBuf::from(cfg.mod_heartbeat_file.trim());
    }
    resolve_state_dir(cfg, state_file).join("autoresolve-mod-heartbeat.json")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Heartbeat {
    /// Unix seconds.
    pub ts: f64,
    pub event: String,
    pub ready: bool,
    #[serde(default)]
    pub instance: String,
}

pub fn write_heartbeat(path: &Path, hb: &Heartbeat) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(hb).unwrap_or_default() + "\n")?;
    std::fs::rename(&tmp, path)
}

/// Pure freshness test: ready, last event not `session.end`, young enough.
pub fn heartbeat_is_fresh(hb: &Heartbeat, now_secs: f64, max_age_secs: u64) -> bool {
    hb.ready
        && hb.event != "session.end"
        && now_secs >= hb.ts
        && now_secs - hb.ts <= max_age_secs as f64
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// True when `prefer_mod` is on, the mode is not off, and the mod's
/// heartbeat is fresh. The tmux path uses it to give the mod first dibs.
pub fn mod_is_fresh(cfg: &AutoResolveConfig, state_file: &str) -> bool {
    if !cfg.prefer_mod {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(heartbeat_path(cfg, state_file)) else {
        return false;
    };
    match serde_json::from_str::<Heartbeat>(&text) {
        Ok(hb) => heartbeat_is_fresh(&hb, now_secs(), cfg.mod_heartbeat_max_age_secs),
        Err(_) => false,
    }
}

/// Entries of the audit file as `(unix_secs, kind)`, newest last, from the
/// last 64 KiB only.
fn recent_audit(state_dir: &Path) -> Vec<(f64, String)> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(state_dir.join(autoresolve::AUDIT_FILE)) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(64 * 1024);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    buf.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let ts = chrono::DateTime::parse_from_rfc3339(v.get("ts")?.as_str()?).ok()?;
            let kind = v.get("kind")?.as_str()?.to_string();
            Some((ts.timestamp_millis() as f64 / 1000.0, kind))
        })
        .collect()
}

/// Rate-limit state derived from the audit file, shared by every decider
/// process and by the daemon's tmux path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    Ok,
    /// Inside a trip cooldown, or this answer would exceed the limit.
    Tripped {
        newly: bool,
    },
}

pub fn rate_state(
    entries: &[(f64, String)],
    now: f64,
    max_per_minute: usize,
    cooldown_secs: u64,
) -> Rate {
    if entries
        .iter()
        .any(|(t, k)| k == "tripped" && now - *t < cooldown_secs as f64)
    {
        return Rate::Tripped { newly: false };
    }
    let answers = entries
        .iter()
        .filter(|(t, k)| (k == "answering" || k == "mod-allow") && now - *t < 60.0)
        .count();
    if answers >= max_per_minute {
        Rate::Tripped { newly: true }
    } else {
        Rate::Ok
    }
}

/// True when the audit trail says the feature is inside a trip cooldown.
pub fn tripped_recently(cfg: &AutoResolveConfig, state_file: &str) -> bool {
    let dir = resolve_state_dir(cfg, state_file);
    matches!(
        rate_state(
            &recent_audit(&dir),
            now_secs(),
            usize::MAX,
            cfg.trip_cooldown_secs
        ),
        Rate::Tripped { .. }
    )
}

/// The single decision entry point of the mod path. Side effects: audit
/// lines and claude-events (never in `Abstain` for off/killed).
pub fn decide(cfg: &AutoResolveConfig, state_file: &str, req: &ModRequest) -> ModAnswer {
    let mode = cfg.parsed_mode();
    let state_dir = resolve_state_dir(cfg, state_file);
    if mode == Mode::Off {
        return abstain("mode off");
    }
    if kill_switch_active(&state_dir) {
        return abstain("kill switch");
    }
    if !cfg.prefer_mod {
        return abstain("prefer_mod disabled");
    }
    if !cfg.default_answer.eq_ignore_ascii_case("no") {
        return abstain("default_answer is not no");
    }

    let policy = Policy::load(&cfg.rules_file);
    let verdict = if req.command.len() > MAX_COMMAND_BYTES {
        Verdict::No {
            reason: "command-too-long".into(),
            command: req.command.chars().take(300).collect(),
            agent: req.agent_id.clone(),
        }
    } else {
        decide_command_text(
            policy.as_ref().map_err(|e| e.as_str()),
            &req.command,
            req.agent_id.as_deref(),
            req.warning.as_deref(),
            None,
        )
    };

    let entry = |kind: &str, answer: &str, rule_ids: &[String], reason: &str| {
        serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": kind,
            "path": "mod",
            "agent_id": req.agent_id,
            "command": req.command,
            "warning": req.warning,
            "rule_ids": rule_ids,
            "reason": reason,
            "answer": answer,
            "mode": mode_str(mode),
        })
    };

    match verdict {
        Verdict::Yes { rule_ids, .. } => {
            if mode == Mode::DryRun {
                audit(
                    &state_dir,
                    entry("mod-would-allow", "yes", &rule_ids, "dry-run"),
                );
                emit_audit_event(
                    "autoresolve-dry-run",
                    &format!(
                        "autoresolve DRY-RUN (mod): would allow via {:?}: {}",
                        rule_ids, req.command
                    ),
                );
                return abstain("dry-run");
            }
            match rate_state(
                &recent_audit(&state_dir),
                now_secs(),
                cfg.max_per_minute,
                cfg.trip_cooldown_secs,
            ) {
                Rate::Ok => {}
                Rate::Tripped { newly } => {
                    if newly {
                        audit(&state_dir, entry("tripped", "none", &[], "rate limit"));
                        emit_trip_event(cfg);
                    }
                    return abstain("rate limit tripped");
                }
            }
            // No audit trail, no allow.
            if !audit(
                &state_dir,
                entry("mod-allow", "yes", &rule_ids, "rule-match"),
            ) {
                return abstain("audit log not writable");
            }
            emit_audit_event(
                "autoresolve-answered",
                &format!(
                    "autoresolve (mod): allowed via {:?}: {}",
                    rule_ids, req.command
                ),
            );
            ModAnswer::Allow { rule_ids }
        }
        Verdict::No {
            reason,
            command,
            agent,
        } => {
            let note = refusal_note(&command, &reason);
            if mode == Mode::DryRun {
                audit(
                    &state_dir,
                    entry("mod-would-deny", "no", &["default-no".into()], &reason),
                );
                emit_audit_event(
                    "autoresolve-dry-run",
                    &format!(
                        "autoresolve DRY-RUN (mod): would deny ({}): {}",
                        reason, command
                    ),
                );
                return abstain("dry-run");
            }
            if !audit(
                &state_dir,
                entry("mod-deny", "no", &["default-no".into()], &reason),
            ) {
                return abstain("audit log not writable");
            }
            emit_denial_event(
                agent.as_deref(),
                req.agent_id.as_deref(),
                true,
                req.warning.as_deref(),
                &reason,
                &command,
            );
            ModAnswer::Deny { reason, note }
        }
    }
}

fn emit_trip_event(cfg: &AutoResolveConfig) {
    if cfg!(test) {
        return;
    }
    let msg = format!(
        "claude-watch: autoresolve (mod) allowed {} commands inside a minute; paused for {}s. \
         Review the autoresolve audit log before re-enabling.",
        cfg.max_per_minute, cfg.trip_cooldown_secs
    );
    crate::event_bus::emit(&crate::event_bus::ClaudeWatchAlert {
        alert_type: "autoresolve-tripped",
        stuck_reason: "autoresolve rate limit exceeded",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: crate::event_bus::Severity::High,
        message: &msg,
    });
}

/// `claude-watch autoresolve-mod decide`: JSON request on stdin, JSON answer
/// on stdout. Always exits 0 with a parseable answer (errors abstain).
pub fn run_decide(stdin_text: &str) -> String {
    let answer = match serde_json::from_str::<ModRequest>(stdin_text) {
        Err(e) => abstain(format!("bad request: {e}")),
        Ok(req) => match crate::config::try_load_config() {
            Err(e) => abstain(format!("config: {e}")),
            Ok(c) => decide(&c.autoresolve, &c.general.state_file, &req),
        },
    };
    serde_json::to_string(&answer).unwrap_or_else(|_| "{\"answer\":\"abstain\"}".into())
}

/// `claude-watch autoresolve-mod heartbeat`.
pub fn run_heartbeat(instance: &str, event: &str, ready: bool) -> Result<(), String> {
    let c = crate::config::try_load_config()?;
    let path = heartbeat_path(&c.autoresolve, &c.general.state_file);
    write_heartbeat(
        &path,
        &Heartbeat {
            ts: now_secs(),
            event: event.to_string(),
            ready,
            instance: instance.to_string(),
        },
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cw-armod-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(dir: &Path, rules: &Path, mode: &str) -> AutoResolveConfig {
        AutoResolveConfig {
            mode: mode.into(),
            rules_file: rules.display().to_string(),
            state_dir: dir.display().to_string(),
            ..Default::default()
        }
    }

    fn rules(dir: &Path) -> PathBuf {
        let p = dir.join("rules.toml");
        std::fs::write(
            &p,
            format!(
                "[[root]]\npath = \"{}\"\nmin_depth = 1\n\n[[rule]]\nid = \"rm-file\"\nanswer = 1\nprograms = [\"rm\"]\nflags = [\"-f\"]\nrequire_subagent = true\n",
                dir.join("work").display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("work")).unwrap();
        p
    }

    fn req(cmd: &str, agent: Option<&str>) -> ModRequest {
        ModRequest {
            command: cmd.into(),
            agent_id: agent.map(String::from),
            warning: None,
        }
    }

    #[test]
    fn allow_listed_command_is_allowed_everything_else_denied() {
        let d = tmpdir("allow");
        let r = rules(&d);
        let c = cfg(&d, &r, "enforce");
        let sf = d.join("state.json").display().to_string();
        let ok = format!("rm -f {}/work/x.tmp", d.display());
        assert!(matches!(
            decide(&c, &sf, &req(&ok, Some("a1b2c3d4e5f6a7b8c"))),
            ModAnswer::Allow { .. }
        ));
        // Same command from the main loop: rule requires a sub-agent.
        assert!(matches!(
            decide(&c, &sf, &req(&ok, None)),
            ModAnswer::Deny { .. }
        ));
        // Outside the roots, heredoc, hard-deny class, recursive: all No.
        for cmd in [
            "rm -f /etc/passwd".to_string(),
            format!(
                "cat <<'EOF' > {}/work/a\nhi\nEOF\nrm -f {}/work/a",
                d.display(),
                d.display()
            ),
            "git push --force origin main".to_string(),
            format!("rm -rf {}/work", d.display()),
        ] {
            match decide(&c, &sf, &req(&cmd, Some("a1b2c3d4e5f6a7b8c"))) {
                ModAnswer::Deny { note, .. } => assert!(note.contains("answered No"), "{note}"),
                other => panic!("{cmd}: {other:?}"),
            }
        }
        let audit_text = std::fs::read_to_string(d.join(autoresolve::AUDIT_FILE)).unwrap();
        assert!(audit_text.contains("mod-allow") && audit_text.contains("mod-deny"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn off_killed_dry_run_and_prefer_mod_false_abstain() {
        let d = tmpdir("abstain");
        let r = rules(&d);
        let sf = d.join("state.json").display().to_string();
        let ok = format!("rm -f {}/work/x.tmp", d.display());
        let rq = req(&ok, Some("a1b2c3d4e5f6a7b8c"));
        assert!(matches!(
            decide(&cfg(&d, &r, "off"), &sf, &rq),
            ModAnswer::Abstain { .. }
        ));
        assert!(matches!(
            decide(&cfg(&d, &r, "dry-run"), &sf, &rq),
            ModAnswer::Abstain { .. }
        ));
        let mut c = cfg(&d, &r, "enforce");
        c.prefer_mod = false;
        assert!(matches!(decide(&c, &sf, &rq), ModAnswer::Abstain { .. }));
        let c = cfg(&d, &r, "enforce");
        std::fs::write(d.join(autoresolve::KILL_SWITCH_FILE), "").unwrap();
        assert!(matches!(decide(&c, &sf, &rq), ModAnswer::Abstain { .. }));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_rules_deny_and_rate_limit_trips() {
        let d = tmpdir("rate");
        let sf = d.join("state.json").display().to_string();
        let missing = d.join("nope.toml");
        let c = cfg(&d, &missing, "enforce");
        assert!(matches!(
            decide(&c, &sf, &req("echo hi", Some("a1b2c3d4e5f6a7b8c"))),
            ModAnswer::Deny { .. }
        ));
        let r = rules(&d);
        let mut c = cfg(&d, &r, "enforce");
        c.max_per_minute = 2;
        let ok = format!("rm -f {}/work/x.tmp", d.display());
        let rq = req(&ok, Some("a1b2c3d4e5f6a7b8c"));
        assert!(matches!(decide(&c, &sf, &rq), ModAnswer::Allow { .. }));
        assert!(matches!(decide(&c, &sf, &rq), ModAnswer::Allow { .. }));
        // Third inside the minute trips; then the cooldown keeps it tripped.
        assert!(matches!(decide(&c, &sf, &rq), ModAnswer::Abstain { .. }));
        assert!(tripped_recently(&c, &sf));
        assert!(matches!(decide(&c, &sf, &rq), ModAnswer::Abstain { .. }));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rate_state_pure() {
        let e = |t: f64, k: &str| (t, k.to_string());
        assert_eq!(rate_state(&[], 100.0, 6, 900), Rate::Ok);
        let many: Vec<_> = (0..6)
            .map(|i| e(95.0 + i as f64 * 0.1, "mod-allow"))
            .collect();
        assert_eq!(
            rate_state(&many, 100.0, 6, 900),
            Rate::Tripped { newly: true }
        );
        assert_eq!(rate_state(&many, 200.0, 6, 900), Rate::Ok);
        let t = [e(50.0, "tripped")];
        assert_eq!(
            rate_state(&t, 100.0, 6, 900),
            Rate::Tripped { newly: false }
        );
        assert_eq!(rate_state(&t, 2000.0, 6, 900), Rate::Ok);
    }

    #[test]
    fn heartbeat_freshness() {
        let hb = |ts: f64, ev: &str, ready: bool| Heartbeat {
            ts,
            event: ev.into(),
            ready,
            instance: "i".into(),
        };
        assert!(heartbeat_is_fresh(&hb(100.0, "tick", true), 130.0, 60));
        assert!(!heartbeat_is_fresh(&hb(100.0, "tick", true), 200.0, 60));
        assert!(!heartbeat_is_fresh(&hb(100.0, "tick", false), 110.0, 60));
        assert!(!heartbeat_is_fresh(
            &hb(100.0, "session.end", true),
            110.0,
            60
        ));
    }

    #[test]
    fn heartbeat_roundtrip_and_mod_is_fresh() {
        let d = tmpdir("hb");
        let sf = d.join("state.json").display().to_string();
        let mut c = AutoResolveConfig {
            state_dir: d.display().to_string(),
            ..Default::default()
        };
        assert!(!mod_is_fresh(&c, &sf));
        let p = heartbeat_path(&c, &sf);
        assert!(p.ends_with("autoresolve-mod-heartbeat.json"));
        write_heartbeat(
            &p,
            &Heartbeat {
                ts: now_secs(),
                event: "tick".into(),
                ready: true,
                instance: "x".into(),
            },
        )
        .unwrap();
        assert!(mod_is_fresh(&c, &sf));
        c.prefer_mod = false;
        assert!(!mod_is_fresh(&c, &sf));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn default_config_prefers_the_mod() {
        let c = AutoResolveConfig::default();
        assert!(c.prefer_mod);
        assert_eq!(c.mod_heartbeat_max_age_secs, 60);
        let parsed: AutoResolveConfig = toml::from_str("mode = \"enforce\"").unwrap();
        assert!(parsed.prefer_mod);
        let off: AutoResolveConfig = toml::from_str("prefer_mod = false").unwrap();
        assert!(!off.prefer_mod);
    }

    #[test]
    fn run_decide_bad_input_abstains() {
        let out = run_decide("not json");
        assert!(out.contains("\"abstain\""), "{out}");
    }
}
