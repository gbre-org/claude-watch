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
//!
//! The same file also corroborates the REACTIVE path's 401 banner
//! (`AccessTokenState` / `read_access_token`). That one keys on the OTHER
//! field — the short-lived `expiresAt` access token — because "OAuth access
//! token has expired" is precisely the failure Claude Code reports when the
//! silent refresh did not happen and the access token on disk is now stale.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Width of the warning window, in milliseconds (three days).
pub const WARNING_WINDOW_MS: i64 = 3 * 24 * 60 * 60 * 1000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

// No `Debug` on either struct: `OauthBlock` carries a bearer token and must
// never be formattable into a log line, even by accident.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthBlock>,
}

#[derive(Deserialize)]
struct OauthBlock {
    #[serde(rename = "refreshTokenExpiresAt")]
    refresh_token_expires_at: Option<i64>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    /// Only its PRESENCE is ever looked at. The value is a bearer token and
    /// must never reach a log line, which is also why this struct does not
    /// derive `Debug`.
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
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
    let Some(oauth) = read_oauth(path) else {
        return CredentialExpiry::Unknown;
    };
    let now_ms = chrono::Local::now().timestamp_millis();
    classify(oauth.refresh_token_expires_at, oauth.expires_at, now_ms)
}

/// The raw `refreshTokenExpiresAt`, for callers that need to notice it MOVING
/// rather than where it currently sits.
///
/// This matters more than it looks. A deployment's refresh token can be
/// short-lived and rolling — measured on one live host, a lifetime of under
/// five hours, silently renewed long before it lapses. Against a three-day
/// warning window such a credential reads as "expires in 1 day" permanently,
/// every second of every day, while being perfectly healthy. The value's
/// POSITION cannot tell those two situations apart; the value MOVING FORWARD
/// can, because that is renewal happening.
pub fn read_refresh_expiry_ms(path: &Path) -> Option<i64> {
    read_oauth(path)?.refresh_token_expires_at
}

/// What the credential store says about the short-lived ACCESS token.
///
/// This is the reactive 401 banner's corroboration, and it is deliberately a
/// different question from `CredentialExpiry`. That one asks "is the LOGIN
/// about to lapse?" and reads `refreshTokenExpiresAt`. This one asks "is the
/// token Claude Code is sending RIGHT NOW dead?" and reads `expiresAt`. Claude
/// Code refreshes the access token silently, so on a healthy session the
/// on-disk `expiresAt` is always in the future; an `expiresAt` in the past
/// means the refresh did not happen, which is exactly the state that makes
/// Claude Code print "OAuth access token has expired".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessTokenState {
    /// The file could not be read or held no OAuth block at all. UNKNOWN,
    /// never a negative.
    Unknown,
    /// There is an access token and its `expiresAt` is in the future.
    Valid,
    /// There is an access token but its `expiresAt` is in the past — or the
    /// file carries no `expiresAt` for it at all, which no healthy Claude
    /// Code write produces.
    Expired,
    /// The OAuth block exists but has no access token in it. Nothing to send
    /// means every request 401s.
    Missing,
}

impl AccessTokenState {
    /// Does this state CORROBORATE an on-screen "access token has expired"?
    /// Only the two states that positively say "there is no usable token".
    /// `Unknown` is not evidence either way and `Valid` is a contradiction.
    pub fn corroborates_401(self) -> bool {
        matches!(self, AccessTokenState::Expired | AccessTokenState::Missing)
    }

    /// Stable lowercase label for logs and JSONL events.
    pub fn as_str(self) -> &'static str {
        match self {
            AccessTokenState::Unknown => "unknown",
            AccessTokenState::Valid => "valid",
            AccessTokenState::Expired => "expired",
            AccessTokenState::Missing => "missing",
        }
    }
}

/// Classify an access token's presence and `expiresAt` against `now`.
///
/// Split out from the file read so the decision is testable without a
/// filesystem or a clock. `has_access_token` is the PRESENCE of the field,
/// never its value.
pub fn classify_access(
    has_access_token: bool,
    expires_at: Option<i64>,
    now_ms: i64,
) -> AccessTokenState {
    if !has_access_token {
        return AccessTokenState::Missing;
    }
    match expires_at {
        Some(exp) if exp > now_ms => AccessTokenState::Valid,
        // A token with no expiry stamped next to it is not a shape Claude
        // Code writes; treat it as unusable rather than inventing a lifetime.
        _ => AccessTokenState::Expired,
    }
}

/// Read the credential store and classify its ACCESS token against the clock.
pub fn read_access_token(path: &Path) -> AccessTokenState {
    let Some(oauth) = read_oauth(path) else {
        return AccessTokenState::Unknown;
    };
    let now_ms = chrono::Local::now().timestamp_millis();
    classify_access(oauth.access_token.is_some(), oauth.expires_at, now_ms)
}

fn read_oauth(path: &Path) -> Option<OauthBlock> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<CredentialsFile>(&raw)
        .ok()?
        .claude_ai_oauth
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

    /// The rolling-credential case, from a real host: a refresh token with a
    /// sub-five-hour life sits permanently inside a three-day window. Its
    /// classification is "expiring" and always will be, which is exactly why
    /// nothing may treat that classification as a standalone trigger.
    #[test]
    fn a_short_lived_rolling_token_reads_as_expiring_forever() {
        let life = (4.8 * 60.0 * 60.0 * 1000.0) as i64;
        // Freshly renewed, mid-life, and nearly due all classify identically.
        for age in [0, life / 2, life - 60_000] {
            assert_eq!(
                classify(Some(NOW + life - age), None, NOW),
                CredentialExpiry::Expiring { days_left: 1 },
                "age {age}ms"
            );
        }
    }

    #[test]
    fn the_raw_refresh_expiry_is_readable_for_movement_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"refreshTokenExpiresAt":1787101726751}}"#,
        )
        .unwrap();
        assert_eq!(read_refresh_expiry_ms(&path), Some(1_787_101_726_751));
        assert_eq!(read_refresh_expiry_ms(&dir.path().join("nope.json")), None);
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

    // ---- Access-token state (the reactive 401 banner's corroboration) ----

    #[test]
    fn a_future_expires_at_is_a_valid_access_token() {
        assert_eq!(
            classify_access(true, Some(NOW + 60_000), NOW),
            AccessTokenState::Valid
        );
        assert!(!AccessTokenState::Valid.corroborates_401());
    }

    #[test]
    fn a_past_expires_at_is_an_expired_access_token() {
        assert_eq!(
            classify_access(true, Some(NOW - 1), NOW),
            AccessTokenState::Expired
        );
        // Exactly now is already gone: Claude Code would be sending a token
        // the server no longer honours.
        assert_eq!(
            classify_access(true, Some(NOW), NOW),
            AccessTokenState::Expired
        );
        assert!(AccessTokenState::Expired.corroborates_401());
    }

    #[test]
    fn a_token_with_no_expiry_stamp_is_treated_as_expired() {
        assert_eq!(classify_access(true, None, NOW), AccessTokenState::Expired);
    }

    #[test]
    fn a_missing_access_token_corroborates_a_401() {
        assert_eq!(
            classify_access(false, Some(NOW + DAY_MS), NOW),
            AccessTokenState::Missing
        );
        assert!(AccessTokenState::Missing.corroborates_401());
    }

    #[test]
    fn an_unreadable_store_is_an_unknown_access_token() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_access_token(&dir.path().join("nope.json")),
            AccessTokenState::Unknown
        );
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(read_access_token(&path), AccessTokenState::Unknown);
        assert!(!AccessTokenState::Unknown.corroborates_401());
    }

    /// The two shapes that matter, written the way Claude Code writes them:
    /// a healthy session (access token hours out, refresh token weeks out) and
    /// the incident shape (refresh token still weeks out, access token STALE
    /// on disk because the silent refresh did not happen). The refresh-token
    /// classification must not move between them — that is the proactive
    /// path's signal, and it was "healthy" throughout the incident.
    #[test]
    fn read_access_token_reads_the_real_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let now = chrono::Local::now().timestamp_millis();
        let refresh = now + 30 * DAY_MS;
        let write = |expires_at: i64| {
            std::fs::write(
                &path,
                format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-x","refreshToken":"sk-ant-ort01-x","expiresAt":{expires_at},"refreshTokenExpiresAt":{refresh},"subscriptionType":"max"}}}}"#
                ),
            )
            .unwrap();
        };
        write(now + 8 * 60 * 60 * 1000);
        assert_eq!(read_access_token(&path), AccessTokenState::Valid);
        assert_eq!(read(&path), CredentialExpiry::Healthy);

        write(now - 60_000);
        assert_eq!(read_access_token(&path), AccessTokenState::Expired);
        assert_eq!(read(&path), CredentialExpiry::Healthy);

        // No access token at all.
        std::fs::write(
            &path,
            format!(r#"{{"claudeAiOauth":{{"refreshTokenExpiresAt":{refresh}}}}}"#),
        )
        .unwrap();
        assert_eq!(read_access_token(&path), AccessTokenState::Missing);
    }
}
