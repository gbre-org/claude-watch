//! `claude-watch hook-fire <type>` CLI subcommand.
//!
//! Invoked by Claude Code hooks (SessionStart / Stop / PreCompact) to fire a
//! reminder into the current conversation. Writes a marker to
//! `~/.cache/claude-watch/reminders/<type>.json` so the daemon knows to
//! defer its heavy-handed fallback (tmux inject) for a grace period.
//!
//! Hook output protocol: Claude Code hook JSON validation is per-event.
//! Only `UserPromptSubmit`, `PostToolUse`, and `SessionStart` accept
//! `hookSpecificOutput.additionalContext` to inject text into the
//! conversation. `Stop`, `PreCompact`, `Notification`, and `PreToolUse`
//! reject that shape — for those events we emit `systemMessage` (a
//! top-level string field that surfaces the reminder to the user as a
//! system message) instead. `PreCompact` can additionally set
//! `continue: false` + `stopReason` to block the compaction.
//!
//! Exit code is always 0 so a hook failure never breaks a Claude session.

use crate::config::try_load_config;
use crate::reminders::{record_fire, ReminderType};
use crate::status;

/// Hook events that accept `hookSpecificOutput.additionalContext` per the
/// Claude Code hook JSON schema. Other events must use `systemMessage`.
fn event_supports_additional_context(event_name: &str) -> bool {
    matches!(
        event_name,
        "UserPromptSubmit" | "PostToolUse" | "SessionStart"
    )
}

/// Grace threshold below which the "context high" reminder should fire,
/// expressed as a percentage of max_context_tokens. 80% matches the
/// design doc.
const CONTEXT_HIGH_THRESHOLD_PCT: f64 = 80.0;

/// Return the context-usage percentage from the current Claude Code
/// status, or None if we can't parse it.
async fn context_usage_pct() -> Option<(u64, u64, f64)> {
    // Fall back to 1M tokens if we can't load a config — hooks must not
    // fail just because the daemon hasn't been set up yet.
    let max = try_load_config()
        .map(|c| c.claude.max_context_tokens)
        .unwrap_or(1_000_000)
        .max(1);
    let cs = status::get_claude_status().await?;
    let pct = (cs.tokens as f64 / max as f64) * 100.0;
    Some((cs.tokens, max, pct))
}

/// Build a hook JSON response that surfaces `text` to the conversation in
/// the schema-correct shape for `hook_event_name`.
///
/// Events that accept `hookSpecificOutput.additionalContext`
/// (`UserPromptSubmit`, `PostToolUse`, `SessionStart`) get the nested
/// shape. All other events (`Stop`, `PreCompact`, `Notification`,
/// `PreToolUse`) get a top-level `systemMessage` string, which is the only
/// conversation-surfacing field those events accept.
fn reminder_response(hook_event_name: &str, text: &str) -> serde_json::Value {
    if event_supports_additional_context(hook_event_name) {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event_name,
                "additionalContext": text,
            }
        })
    } else {
        serde_json::json!({
            "systemMessage": text,
        })
    }
}

/// Build a `continue: false` response that blocks the hook's event and
/// shows `stop_reason` + a reminder back to the user. Used by PreCompact
/// to ask Claude to `/clear` instead of auto-compacting.
///
/// The reminder text is attached via the schema-correct field for the
/// event: `hookSpecificOutput.additionalContext` where supported, else
/// top-level `systemMessage`.
fn block_with_context(hook_event_name: &str, stop_reason: &str, text: &str) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("continue".into(), serde_json::Value::Bool(false));
    obj.insert(
        "stopReason".into(),
        serde_json::Value::String(stop_reason.to_string()),
    );
    if event_supports_additional_context(hook_event_name) {
        obj.insert(
            "hookSpecificOutput".into(),
            serde_json::json!({
                "hookEventName": hook_event_name,
                "additionalContext": text,
            }),
        );
    } else {
        obj.insert(
            "systemMessage".into(),
            serde_json::Value::String(text.to_string()),
        );
    }
    serde_json::Value::Object(obj)
}

/// Empty JSON — hook does nothing. Used when the trigger condition isn't
/// met (e.g. context is fine on Stop, versions match on SessionStart).
fn noop_response() -> serde_json::Value {
    serde_json::json!({})
}

/// CLI entry point for `claude-watch hook-fire <type> [--hook-event NAME]`.
///
/// Prints a hook JSON response to stdout. Always exits 0 — a broken hook
/// must not break Claude Code sessions.
pub async fn cmd_hook_fire(kind_str: &str, hook_event: Option<&str>) -> i32 {
    let kind = match ReminderType::from_str(kind_str) {
        Some(k) => k,
        None => {
            // Unknown type — emit empty response so the hook is a no-op.
            // We deliberately don't error to stderr here because some
            // hooks capture stderr.
            println!("{}", noop_response());
            return 0;
        }
    };

    let response = match kind {
        ReminderType::ContextHigh => handle_context_high(hook_event).await,
        ReminderType::VersionUpdate => handle_version_update(hook_event).await,
        ReminderType::PreCompact => handle_pre_compact(hook_event).await,
    };

    println!("{}", response);
    0
}

async fn handle_context_high(hook_event: Option<&str>) -> serde_json::Value {
    let event_name = hook_event.unwrap_or("Stop");
    let (tokens, max, pct) = match context_usage_pct().await {
        Some(v) => v,
        None => return noop_response(),
    };

    if pct < CONTEXT_HIGH_THRESHOLD_PCT {
        return noop_response();
    }

    let text = format!(
        "[claude-watch] Context usage is at {:.0}% ({}k / {}k tokens). \
         Consider running `/clear` before continuing — the daemon will \
         force a self-clear if usage stays high.",
        pct,
        tokens / 1000,
        max / 1000,
    );

    record_fire(
        ReminderType::ContextHigh,
        Some(serde_json::json!({"tokens": tokens, "max": max, "pct": pct})),
    );

    reminder_response(event_name, &text)
}

async fn handle_version_update(hook_event: Option<&str>) -> serde_json::Value {
    let event_name = hook_event.unwrap_or("SessionStart");
    // Pane-scoped, mirroring the auto-update daemon path
    // (`policy::check_auto_update` / `check_update_trigger`): resolve the
    // main-loop pane and read the running version off THAT pane's PID rather
    // than the first PID an unscoped `pgrep -af claude` happens to match,
    // which PID reuse can fool (see `status::resolve_running_version`'s
    // session-marker hardening). Falls back to the unscoped resolver only
    // when no pane can be found at all (fresh install / no tmux running).
    let tmux_cfg = try_load_config().map(|c| c.tmux).unwrap_or_default();
    let info = match status::find_claude_pane_with_config(&tmux_cfg).await {
        Some(pane) => status::get_version_info_for_pane(&pane).await,
        None => tokio::task::spawn_blocking(status::get_version_info)
            .await
            .unwrap_or_default(),
    };

    let running = match info.running {
        Some(v) => v,
        None => return noop_response(),
    };
    let installed = match info.installed {
        Some(v) => v,
        None => return noop_response(),
    };

    if running == installed {
        return noop_response();
    }

    let text = format!(
        "[claude-watch] A newer Claude Code is installed: {} -> {}. \
         Run `/restart` (or exit and re-launch) to pick it up. The daemon \
         will fall back to `claude update` if the mismatch persists.",
        running, installed,
    );

    record_fire(
        ReminderType::VersionUpdate,
        Some(serde_json::json!({"running": running, "installed": installed})),
    );

    reminder_response(event_name, &text)
}

async fn handle_pre_compact(hook_event: Option<&str>) -> serde_json::Value {
    let event_name = hook_event.unwrap_or("PreCompact");
    // Record the fire regardless of context usage — the hook only runs on
    // `auto` matcher, so every invocation is a "Claude is about to auto-
    // compact" event we want to track.
    record_fire(ReminderType::PreCompact, None);

    let text = "[claude-watch] Auto-compaction is about to run. Consider \
        running `/clear` instead for a cleaner reset. If `/clear` isn't \
        appropriate, let compaction proceed.";

    block_with_context(
        event_name,
        "Auto-compaction blocked by claude-watch — consider /clear instead.",
        text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events that accept `hookSpecificOutput.additionalContext`.
    const ADDITIONAL_CONTEXT_EVENTS: &[&str] =
        &["UserPromptSubmit", "PostToolUse", "SessionStart"];

    /// Events that must use top-level `systemMessage` instead.
    const SYSTEM_MESSAGE_EVENTS: &[&str] =
        &["Stop", "PreCompact", "Notification", "PreToolUse"];

    /// Validate that a hook JSON response conforms to the Claude Code hook
    /// schema: no top-level unknown fields, `hookSpecificOutput` (when
    /// present) is an object with `hookEventName`, and `additionalContext`
    /// only appears on supported events.
    fn assert_schema_compliant(v: &serde_json::Value, event: &str) {
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("response for {event} must be a JSON object"));

        // Allowed top-level fields per the schema.
        let allowed_top_level: &[&str] = &[
            "continue",
            "suppressOutput",
            "stopReason",
            "decision",
            "reason",
            "systemMessage",
            "permissionDecision",
            "hookSpecificOutput",
        ];
        for key in obj.keys() {
            assert!(
                allowed_top_level.contains(&key.as_str()),
                "unexpected top-level key {key:?} for event {event}"
            );
        }

        if let Some(hso) = obj.get("hookSpecificOutput") {
            let hso_obj = hso
                .as_object()
                .unwrap_or_else(|| panic!("hookSpecificOutput for {event} must be an object"));
            assert_eq!(
                hso_obj
                    .get("hookEventName")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
                event,
                "hookEventName must match the event for {event}"
            );
            if hso_obj.contains_key("additionalContext") {
                assert!(
                    ADDITIONAL_CONTEXT_EVENTS.contains(&event),
                    "event {event} must not use additionalContext"
                );
            }
        }

        if obj.contains_key("systemMessage") {
            assert!(
                obj.get("systemMessage")
                    .and_then(|v| v.as_str())
                    .is_some(),
                "systemMessage must be a string for {event}"
            );
        }
    }

    #[test]
    fn event_classification() {
        for e in ADDITIONAL_CONTEXT_EVENTS {
            assert!(
                event_supports_additional_context(e),
                "{e} should support additionalContext"
            );
        }
        for e in SYSTEM_MESSAGE_EVENTS {
            assert!(
                !event_supports_additional_context(e),
                "{e} should NOT support additionalContext"
            );
        }
    }

    #[test]
    fn reminder_response_additional_context_events() {
        for e in ADDITIONAL_CONTEXT_EVENTS {
            let v = reminder_response(e, "hello");
            assert_schema_compliant(&v, e);
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], *e);
            assert_eq!(
                v["hookSpecificOutput"]["additionalContext"],
                serde_json::Value::String("hello".to_string())
            );
            assert!(
                v.get("systemMessage").is_none(),
                "{e} should not use systemMessage"
            );
        }
    }

    #[test]
    fn reminder_response_system_message_events() {
        for e in SYSTEM_MESSAGE_EVENTS {
            let v = reminder_response(e, "hello");
            assert_schema_compliant(&v, e);
            assert_eq!(
                v["systemMessage"],
                serde_json::Value::String("hello".to_string())
            );
            assert!(
                v.get("hookSpecificOutput").is_none(),
                "{e} must not emit hookSpecificOutput.additionalContext"
            );
        }
    }

    #[test]
    fn block_with_context_additional_context_event() {
        // SessionStart supports additionalContext — the block response
        // should use the nested shape.
        let v = block_with_context("SessionStart", "nope", "try /clear");
        assert_schema_compliant(&v, "SessionStart");
        assert_eq!(v["continue"], serde_json::Value::Bool(false));
        assert_eq!(v["stopReason"], "nope");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            serde_json::Value::String("try /clear".to_string())
        );
        assert!(v.get("systemMessage").is_none());
    }

    #[test]
    fn block_with_context_system_message_event() {
        // PreCompact does NOT support additionalContext — the block
        // response must surface the reminder via systemMessage.
        let v = block_with_context("PreCompact", "nope", "try /clear");
        assert_schema_compliant(&v, "PreCompact");
        assert_eq!(v["continue"], serde_json::Value::Bool(false));
        assert_eq!(v["stopReason"], "nope");
        assert_eq!(
            v["systemMessage"],
            serde_json::Value::String("try /clear".to_string())
        );
        assert!(v.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn block_with_context_stop_event_uses_system_message() {
        // Regression: the original bug was a Stop hook producing
        // hookSpecificOutput.additionalContext, which Claude Code rejects.
        let v = block_with_context("Stop", "stopping", "reminder text");
        assert_schema_compliant(&v, "Stop");
        assert!(v.get("hookSpecificOutput").is_none());
        assert_eq!(
            v["systemMessage"],
            serde_json::Value::String("reminder text".to_string())
        );
    }

    #[test]
    fn noop_shape() {
        let v = noop_response();
        assert!(v.is_object());
        assert_eq!(v.as_object().map(|o| o.is_empty()), Some(true));
    }

    #[test]
    fn stop_event_reminder_matches_bug_report_fix() {
        // Reproduces the failing case from Andrew's 2026-04-17 screenshot:
        // Stop hook with the context_high reminder text. The output MUST
        // NOT contain hookSpecificOutput.additionalContext — it must use
        // top-level systemMessage.
        let text = "[claude-watch] Context usage is at 86% (856k / 1000k tokens). \
                    Consider running `/clear` before continuing.";
        let v = reminder_response("Stop", text);
        assert_schema_compliant(&v, "Stop");
        assert!(
            v.get("hookSpecificOutput").is_none(),
            "Stop hook must not emit hookSpecificOutput"
        );
        assert_eq!(
            v["systemMessage"],
            serde_json::Value::String(text.to_string())
        );
    }
}
