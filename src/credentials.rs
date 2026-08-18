//! On-disk Claude Code OAuth credential expiry.
//!
//! The daemon's proactive login-expiry path needs to answer one question:
//! "is this session's login actually about to lapse?" The pane banner
//! (`tmux::detect_login_expiry_warning`) is the signal the operator asked us
//! to react to, but on its own it is two things at once:
//!
//!   * a genuine warning painted by Claude Code, and
//!   * arbitrary conversation text, because "Your login expires in 2 days"
//!     is a sentence that can appear on the pane simply because somebody is
//!     reading this file, this module's tests, or the pull request that added
//!     them.
//!
//! Firing `/login` at conversation text would park a healthy session in a
//! modal that swallows the loop's keystrokes. So the pane signal is
//! corroborated against the credential store, which is ground truth and
//! cannot be spoofed by anything on screen.
//!
//! It is also the *fallback*: the transient form of Claude Code's warning
//! lives on screen for about fifteen seconds at a time, so a poller can
//! legitimately never see it. Reading the expiry directly closes that hole.
//!
//! The rules below mirror the ones the shipped Claude Code bundle applies
//! before it will render its warning at all — they were read out of the
//! binary, not invented here, so the daemon warns on exactly the window
//! Claude Code warns on:
//!
//!   * the warning window is three days wide;
//!   * `refreshTokenExpiresAt` is the field that matters (the short-lived
//!     `expiresAt` access token is refreshed silently and is not what lapses);
//!   * if `expiresAt` somehow sits more than a window past
//!     `refreshTokenExpiresAt`, the credential shape is not one this warning
//!     understands and nothing is reported;
//!   * an already-expired token reports nothing — that is the REACTIVE
//!     reauth path's job, not this one's.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Width of the warning window, in milliseconds (three days).
pub const WARNING_WINDOW_MS: i64 = 3 * 24 * 60 * 60 * 1000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthBlock>,
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "refreshTokenExpiresAt")]
    refresh_token_expires_at: Option<i64>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

/// Default location of the credential store.
pub fn default_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join(".claude").join(".credentials.json")
}

/// What the credential store says about how much login is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialExpiry {
    /// The file could not be read or held no usable OAuth expiry. This is
    /// UNKNOWN, never "fine" — callers must not read it as a negative.
    Unknown,
    /// Readable, and the login is NOT inside the warning window.
    Healthy,
    /// Readable, and the login lapses within the warning window.
    /// `days_left` is rounded UP, matching how Claude Code renders it.
    Expiring { days_left: u32 },
    /// Readable, and the refresh token has already lapsed. The reactive
    /// reauth path owns this state; the proactive path stands down.
    Expired,
}

/// Classify a `refreshTokenExpiresAt` / `expiresAt` pair against `now`.
///
/// Split out from the file read so the whole decision is testable without a
/// filesystem or a clock.
pub fn classify(
    refresh_token_expires_at: Option<i64>,
    expires_at: Option<i64>,
    now_ms: i64,
) -> CredentialExpiry {
    let Some(refresh_expiry) = refresh_token_expires_at else {
        return CredentialExpiry::Unknown;
    };
    // A credential whose access token outlives its refresh token by more than
    // a whole window is not the shape this warning was written for. Claude
    // Code declines to render anything; so do we.
    if let Some(access_expiry) = expires_at {
        if access_expiry > refresh_expiry + WARNING_WINDOW_MS {
            return CredentialExpiry::Unknown;
        }
    }
    let remaining = refresh_expiry - now_ms;
    if remaining <= 0 {
        return CredentialExpiry::Expired;
    }
    if remaining > WARNING_WINDOW_MS {
        return CredentialExpiry::Healthy;
    }
    // Round up: 30 minutes left is "1 day", never "0 days".
    //
    // `div_ceil` is only stable for UNSIGNED integers (signed division
    // rounding is still behind the unstable `int_roundings` feature), and
    // `remaining` has to be signed so the `<= 0` check above can exist. The
    // cast is sound precisely because that check already ran: everything
    // reaching this line is strictly positive.
    let days_left = (remaining as u64).div_ceil(DAY_MS as u64).max(1) as u32;
    CredentialExpiry::Expiring { days_left }
}

/// Read the credential store and classify it against the current clock.
pub fn read(path: &Path) -> CredentialExpiry {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return CredentialExpiry::Unknown;
    };
    let Ok(parsed) = serde_json::from_str::<CredentialsFile>(&raw) else {
        return CredentialExpiry::Unknown;
    };
    let Some(oauth) = parsed.claude_ai_oauth else {
        return CredentialExpiry::Unknown;
    };
    let now_ms = chrono::Local::now().timestamp_millis();
    classify(oauth.refresh_token_expires_at, oauth.expires_at, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const NOW: i64 = 1_787_000_000_000;

    #[test]
    fn missing_refresh_expiry_is_unknown() {
        assert_eq!(classify(None, Some(NOW), NOW), CredentialExpiry::Unknown);
    }

    #[test]
    fn far_future_expiry_is_healthy() {
        let refresh = NOW + 30 * DAY_MS;
        assert_eq!(
            classify(Some(refresh), Some(NOW + DAY_MS), NOW),
            CredentialExpiry::Healthy
        );
    }

    #[test]
    fn inside_the_window_reports_days_rounded_up() {
        // Just under two days left renders as "2 days", the way Claude Code
        // renders it — a ceiling, not a truncation.
        let refresh = NOW + 2 * DAY_MS - 60_000;
        assert_eq!(
            classify(Some(refresh), None, NOW),
            CredentialExpiry::Expiring { days_left: 2 }
        );
    }

    #[test]
    fn a_sliver_of_time_left_is_one_day_not_zero() {
        assert_eq!(
            classify(Some(NOW + 60_000), None, NOW),
            CredentialExpiry::Expiring { days_left: 1 }
        );
    }

    #[test]
    fn the_window_edge_is_inclusive_and_one_ms_past_it_is_healthy() {
        assert_eq!(
            classify(Some(NOW + WARNING_WINDOW_MS), None, NOW),
            CredentialExpiry::Expiring { days_left: 3 }
        );
        assert_eq!(
            classify(Some(NOW + WARNING_WINDOW_MS + 1), None, NOW),
            CredentialExpiry::Healthy
        );
    }

    #[test]
    fn already_lapsed_is_expired_not_expiring() {
        // The reactive reauth path owns a dead credential. If this returned
        // Expiring, the proactive path would race it into the same modal.
        assert_eq!(classify(Some(NOW - 1), None, NOW), CredentialExpiry::Expired);
        assert_eq!(
            classify(Some(NOW - 10 * DAY_MS), None, NOW),
            CredentialExpiry::Expired
        );
    }

    #[test]
    fn an_access_token_outliving_the_refresh_token_by_a_window_is_unknown() {
        let refresh = NOW + DAY_MS;
        assert_eq!(
            classify(Some(refresh), Some(refresh + WARNING_WINDOW_MS + 1), NOW),
            CredentialExpiry::Unknown
        );
        // ...but merely outliving it by less than a window is a normal
        // credential and still reports.
        assert_eq!(
            classify(Some(refresh), Some(refresh + DAY_MS), NOW),
            CredentialExpiry::Expiring { days_left: 1 }
        );
    }

    #[test]
    fn a_missing_file_is_unknown_not_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert_eq!(read(&path), CredentialExpiry::Unknown);
    }

    #[test]
    fn garbage_and_wrong_shapes_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        for body in ["not json at all", "{}", r#"{"claudeAiOauth": {}}"#] {
            let path = dir.path().join("creds.json");
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(body.as_bytes()).unwrap();
            assert_eq!(read(&path), CredentialExpiry::Unknown, "body: {body}");
        }
    }

    #[test]
    fn a_real_shaped_file_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let soon = chrono::Local::now().timestamp_millis() + DAY_MS;
        let body = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-x","refreshToken":"sk-ant-ort01-x","expiresAt":{soon},"refreshTokenExpiresAt":{soon},"subscriptionType":"max"}}}}"#
        );
        std::fs::write(&path, body).unwrap();
        assert_eq!(read(&path), CredentialExpiry::Expiring { days_left: 1 });
    }
}
