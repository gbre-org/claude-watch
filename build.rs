use std::process::Command;

/// Build-time git stamping.
///
/// Exposes two env vars to the crate so the exporter can emit a
/// `claude_watch_build_info` gauge identifying the deployed build:
///   - `CW_GIT_COMMIT`: short commit hash of HEAD (e.g. `abc1234`).
///   - `CW_GIT_PR`: PR number parsed from the trailing `(#N)` squash-merge
///     convention in the latest commit subject (e.g. `358`), or "" if none.
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

/// Parse the trailing `(#N)` from a squash-merge commit subject.
/// e.g. "feat(x): do thing (#358)" -> Some("358").
fn parse_pr_number(subject: &str) -> Option<String> {
    let subject = subject.trim_end();
    let close = subject.strip_suffix(')')?;
    let open = close.rfind("(#")?;
    let digits = &close[open + 2..];
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        Some(digits.to_string())
    } else {
        None
    }
}
