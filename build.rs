use std::path::Path;
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

    // 2. Fall back to a build-context stamp FILE (container image build).
    //    `container/Dockerfile` writes `.build-commit` / `.build-pr` from the
    //    `--build-arg` values into the crate root inside the SAME `cargo build`
    //    RUN. Reading a source-tree file (with `rerun-if-changed` below) is
    //    robust against the BuildKit `type=cache` mount on `/build/target`,
    //    which persists build.rs's fingerprint AND its cached rustc-env output
    //    across image builds: the file is rewritten every build so its mtime is
    //    always newer than the cached fingerprint, forcing cargo to re-run
    //    build.rs and restamp. `rerun-if-env-changed` alone did NOT force a
    //    re-run here — the stale build-script output in the persisted target
    //    mount was reused, so `claude_watch_build_info` read commit="unknown"
    //    even on a clean layer-cache-pruned rebuild.
    if commit.is_none() {
        commit = read_stamp_file(".build-commit");
    }
    if pr.is_none() {
        pr = read_stamp_file(".build-pr");
    }

    // 3. Fall back to build-arg-injected env vars (bare `docker build`, or a
    //    host `cargo build` with the vars exported). Kept as a secondary path.
    if commit.is_none() {
        commit = build_env("CW_BUILD_COMMIT");
    }
    if pr.is_none() {
        pr = build_env("CW_BUILD_PR");
    }

    // 4. Final fallbacks.
    let commit = commit.unwrap_or_else(|| "unknown".into());
    let pr = pr.unwrap_or_default();

    println!("cargo:rustc-env=CW_GIT_COMMIT={}", commit);
    println!("cargo:rustc-env=CW_GIT_PR={}", pr);

    // Restamp when HEAD moves (new commit / branch switch), and when the
    // container build-context stamp files change.
    //
    // Only paths that EXIST are emitted. Cargo treats a missing
    // `rerun-if-changed` path as perpetually "changed", so a single absent
    // path re-runs this build script — and therefore recompiles and relinks
    // the whole crate — on EVERY `cargo build`, forever. Every candidate
    // below is routinely absent: `.build-commit` / `.build-pr` are gitignored
    // and only ever written inside the container build, `.git` is a FILE (not
    // a directory) in a linked git worktree, and `refs/heads/<branch>` does
    // not exist as a loose ref once refs have been packed.
    for path in rerun_paths() {
        println!("cargo:rerun-if-changed={}", path);
    }
    // Restamp when the build-arg-injected stamp changes (container builds).
    println!("cargo:rerun-if-env-changed=CW_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=CW_BUILD_PR");
}

/// Collect the `rerun-if-changed` paths that actually exist on disk.
///
/// Git paths are resolved with `git rev-parse --git-path`, which is
/// worktree-aware: in a linked worktree `.git` is a file pointing elsewhere,
/// so a hardcoded `.git/HEAD` is both wrong and absent there.
///
/// The existence filter is the important part — see the caller.
fn rerun_paths() -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    // HEAD itself covers branch switches.
    if let Some(head) = git_output(&["rev-parse", "--git-path", "HEAD"]) {
        candidates.push(head);
    }
    // The ref HEAD points at (e.g. refs/heads/main) covers new commits on the
    // current branch while it is a loose ref; `packed-refs` covers the same
    // once that ref has been packed away.
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_output(&["rev-parse", "--git-path", &head_ref]) {
            candidates.push(path);
        }
    }
    if let Some(packed) = git_output(&["rev-parse", "--git-path", "packed-refs"]) {
        candidates.push(packed);
    }

    // Container build-context stamps. The Dockerfile rewrites these every
    // image build, so when they ARE present their mtime is always newer than
    // the build-script fingerprint cached in the persisted `/build/target`
    // mount — that is what forces a re-run and restamp across image builds.
    candidates.push(".build-commit".to_string());
    candidates.push(".build-pr".to_string());

    candidates
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .collect()
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

/// Read a build-context stamp file (e.g. `.build-commit`) from the crate root,
/// treating a missing file or empty / whitespace-only contents as absent so a
/// bare `docker build` (no stamp file, or an empty one) cleanly falls through
/// to the env var and then "unknown".
fn read_stamp_file(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
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
