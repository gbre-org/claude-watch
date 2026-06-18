// Pure parsing of a PR number from a git commit subject.
//
// This file is the single source of truth for `parse_pr_number`. It is both:
//   - declared as a normal crate module (`pub mod pr_parse` in `lib.rs`), so
//     the unit tests below run under `cargo test` / `cargo nextest`; and
//   - `include!`d by `build.rs`, so the build script stamps `CW_GIT_PR` using
//     the exact same code that the tests exercise.
//
// Keep it dependency-free and `std`-only so it works in the `build.rs`
// context (build scripts cannot depend on the crate they build). For that
// reason this uses `//` comments, not `//!` inner doc comments, which are
// illegal when the file is `include!`d partway through `build.rs`.

/// Parse a PR number from a git commit subject.
///
/// Recognizes, in priority order:
///   1. GitHub merge-commit subjects:
///      `Merge pull request #N from <branch>` -> `Some("N")`.
///   2. The trailing squash-merge convention:
///      `... (#N)` -> `Some("N")` (e.g. `feat(x): do thing (#358)`).
///
/// Anything else (plain commits, `Merge branch '...'`, a bare `#N` appearing
/// mid-subject in some other context) -> `None`. Intentionally conservative:
/// only the two canonical shapes above are matched, so issue refs or unrelated
/// `#N` tokens are not mis-extracted.
pub fn parse_pr_number(subject: &str) -> Option<String> {
    let subject = subject.trim();

    // 1. GitHub merge-commit subject: "Merge pull request #N from <branch>".
    const MERGE_PREFIX: &str = "Merge pull request #";
    if let Some(rest) = subject.strip_prefix(MERGE_PREFIX) {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    // 2. Trailing squash-merge convention: "... (#N)".
    let trimmed = subject.trim_end();
    if let Some(close) = trimmed.strip_suffix(')') {
        if let Some(open) = close.rfind("(#") {
            let digits = &close[open + 2..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return Some(digits.to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::parse_pr_number;

    #[test]
    fn merge_commit_subject() {
        assert_eq!(
            parse_pr_number("Merge pull request #367 from hndrewaall/ah/mcp-autoapprove-shim"),
            Some("367".to_string())
        );
    }

    #[test]
    fn squash_trailing_ref() {
        assert_eq!(
            parse_pr_number("feat(x): do thing (#358)"),
            Some("358".to_string())
        );
    }

    #[test]
    fn merge_branch_is_none() {
        assert_eq!(parse_pr_number("Merge branch 'main' into ah/foo"), None);
    }

    #[test]
    fn plain_subject_is_none() {
        assert_eq!(parse_pr_number("fix: tidy up the parser"), None);
    }

    #[test]
    fn mid_text_hash_is_none() {
        // `#123` appears mid-subject, not in either canonical position.
        assert_eq!(
            parse_pr_number("address review feedback from #123 discussion"),
            None
        );
    }

    #[test]
    fn merge_commit_without_branch_suffix() {
        // Defensive: still parse the number even if the "from <branch>" tail
        // is absent, since the digits terminate the run.
        assert_eq!(
            parse_pr_number("Merge pull request #42"),
            Some("42".to_string())
        );
    }

    #[test]
    fn empty_subject_is_none() {
        assert_eq!(parse_pr_number(""), None);
    }
}
