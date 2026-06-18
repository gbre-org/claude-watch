use std::process::Command;

// Shared, dependency-free PR-subject parser. Included verbatim so the build
// script and the crate's test target exercise the exact same `parse_pr_number`.
// (Build scripts cannot depend on the crate they build, hence `include!`.)
include!("src/pr_parse.rs");

/// Build-time git stamping.
///
/// Exposes two env vars to the crate so the exporter can emit a
/// `claude_watch_build_info` gauge identifying the deployed build:
///   - `CW_GIT_COMMIT`: short commit hash of HEAD (e.g. `abc1234`).
///   - `CW_GIT_PR`: PR number parsed from the latest commit subject. Recognizes
///     both GitHub merge-commit subjects (`Merge pull request #N from ...`) and
///     the trailing `(#N)` squash-merge convention; "" if neither matches.
///
/// Robust by design: if git is unavailable or any step fails, falls back to
/// `"unknown"` / `""` so the build never breaks.
fn main() {
    let commit = git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());

    let subject = git_output(&["log", "-1", "--format=%s"]).unwrap_or_default();
    let pr = parse_pr_number(&subject).unwrap_or_default();

    println!("cargo:rustc-env=CW_GIT_COMMIT={}", commit);
    println!("cargo:rustc-env=CW_GIT_PR={}", pr);

    // Restamp when HEAD moves (new commit / branch switch). `.git/HEAD` covers
    // branch changes; the file HEAD points at (e.g. refs/heads/main) covers new
    // commits on the current branch.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/{}", head_ref);
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
