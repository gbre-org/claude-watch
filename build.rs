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
/// Resolution order (first non-empty wins):
///   1. Live git in the build CWD (`git rev-parse` / `git log`). Works for a
///      normal `cargo build` on the host where `.git` is present.
///   2. Build-arg-injected env vars `CW_BUILD_COMMIT` / `CW_BUILD_PR`, read
///      from the build-script environment. The container image build prunes
///      `.git/` from the Docker context (.dockerignore), so step 1 fails
///      inside `container/Dockerfile`; the Dockerfile sets these env vars from
///      `--build-arg` values the Makefile computes on the host, where git IS
///      available. This is what keeps `claude_watch_build_info` from reading
///      `commit="unknown"` on container builds.
///   3. Fallback `"unknown"` / `""` so the build never breaks.
fn main() {
    // 1. Try live git first.
    let mut commit = git_output(&["rev-parse", "--short", "HEAD"]);
    let mut pr = git_output(&["log", "-1", "--format=%s"])
        .and_then(|subject| parse_pr_number(&subject).map(|n| n.to_string()));

    // 2. Fall back to build-arg-injected env vars (container image build).
    if commit.is_none() {
        commit = build_env("CW_BUILD_COMMIT");
    }
    if pr.is_none() {
        pr = build_env("CW_BUILD_PR");
    }

    // 3. Final fallbacks.
    let commit = commit.unwrap_or_else(|| "unknown".into());
    let pr = pr.unwrap_or_default();

    println!("cargo:rustc-env=CW_GIT_COMMIT={}", commit);
    println!("cargo:rustc-env=CW_GIT_PR={}", pr);

    // Restamp when HEAD moves (new commit / branch switch). `.git/HEAD` covers
    // branch changes; the file HEAD points at (e.g. refs/heads/main) covers new
    // commits on the current branch.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/{}", head_ref);
    }
    // Restamp when the build-arg-injected stamp changes (container builds).
    println!("cargo:rerun-if-env-changed=CW_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=CW_BUILD_PR");
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

/// Read a build-script env var, treating empty / whitespace-only as absent so a
/// `--build-arg CW_BUILD_COMMIT=` (empty) cleanly falls through to "unknown".
fn build_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}
