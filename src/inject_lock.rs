//! Cross-process serialization for tmux pane injects.
//!
//! # The bug this exists to prevent
//!
//! Every injector drives `tmux send-keys` against the SAME Claude Code prompt
//! line. Since 2026-08-18 the default inject choreography is NON-CANCELLING:
//! no Escape blast, and — the load-bearing detail — **no `dd` line-clear**. As
//! `tmux::inject_and_verify` documents, that means "half-typed operator input
//! glues onto the payload". It also means *another injector's* half-typed
//! payload glues onto ours, because neither one clears the line.
//!
//! Observed 2026-08-19T10:08:47. `cw-watcher-health-check` was typing
//!
//! ```text
//! [CLAUDE-WATCH] WATCHER DOWN: 3 event(s) unconsumed >6min — the event watcher is dead...
//! ```
//!
//! while `cw-theme-sync` typed `/config theme=light`. The two `send-keys`
//! streams interleaved and what reached the model was
//!
//! ```text
//! unconsumed >6mi/config theme=lightn — the event watcher is dead
//! ```
//!
//! — the theme payload spliced into the middle of the word `6min`. Claude Code
//! never saw a slash command, the theme never changed, and BOTH injectors
//! reported success: a submit is "verified" by the payload CLEARING from the
//! prompt line, which is exactly as true when mangled text gets submitted as
//! when the real thing does.
//!
//! # Why the lock lives here and not in the callers
//!
//! There were two independent racing populations and no single caller could
//! see both:
//!
//!   * **Out-of-process** callers shelling out to `claude-watch inject`
//!     (`cw-theme-sync`, `cw-watcher-health-check`, `self-clear`,
//!     `mcp-reconnect`, `self-login`, `claude-watch-dispatch`).
//!   * **In-process** daemon alerts, which never shell out at all — they call
//!     `tmux::inject_text{,_queued}` directly through
//!     [`crate::inject_dispatch::RealBackends`].
//!
//! A per-caller lock convention cannot cover the second population, and it is
//! forgettable by construction: the next injector added is one that nobody
//! remembers to wire up. So the lock is taken at the two **dispatch
//! boundaries** every inject must pass through — `run_inject` (the CLI
//! subcommand) and `RealBackends` (the daemon) — plus `inject_shell`.
//!
//! It is deliberately NOT taken inside `tmux::inject_text` /
//! `inject_text_queued` / `inject_and_verify`: those nest
//! (`inject_and_verify` with `--escape` on regular text calls `inject_text`),
//! and `flock` on a second open file description from the same process
//! conflicts with the first, so locking the primitives would self-deadlock.
//!
//! # Not the same lock as self-clear's
//!
//! `self-clear` takes `$XDG_RUNTIME_DIR/claude-self-clear.lock` to keep two
//! *self-clears* from overlapping. Reusing it here would be a trap: a systemd
//! **system** unit gets no `XDG_RUNTIME_DIR`, so the daemon would resolve
//! `/var/run/claude/...` while a login-shell caller resolved
//! `/run/user/1000/...` — two different files, and the serialization would
//! silently not exist. (Both of those paths exist on the deployed host today,
//! which is what that divergence looks like in the wild.) This lock therefore
//! uses an ABSOLUTE, env-independent default under the runtime dir
//! claude-watch already owns, so every injector resolves the identical file
//! no matter how it was started.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::{debug, warn};

/// Absolute, env-independent default. `/var/run/claude` is claude-watch's
/// runtime dir (`watcher::PID_DIR`), provisioned uid-1000-owned by
/// `/etc/tmpfiles.d/claude.conf`.
pub const DEFAULT_INJECT_LOCK: &str = "/var/run/claude/claude-inject.lock";

/// Env override. Set to the empty string to DISABLE locking entirely (for
/// sandboxes / test harnesses with no writable runtime dir).
pub const INJECT_LOCK_ENV: &str = "CW_INJECT_LOCK";

/// How long to wait for a peer inject to finish before giving up and going
/// ahead anyway. Generous on purpose: a full verified inject is ~5-15s
/// (Escape coercion + typing + a 3s verify window), so a normal queue of two
/// or three injectors clears well inside this. Exceeding it means an injector
/// is genuinely wedged, and at that point dropping an alert is worse than
/// risking an interleave.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(90);

/// Retry cadence while the lock is contended.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Resolve the lock path. `None` = locking disabled.
pub fn lock_path() -> Option<PathBuf> {
    match std::env::var(INJECT_LOCK_ENV) {
        // Explicitly set to empty => disabled.
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(PathBuf::from(v)),
        Err(_) => Some(PathBuf::from(DEFAULT_INJECT_LOCK)),
    }
}

/// RAII holder for the inject lock.
///
/// The `flock` is released by the kernel when the `File` is dropped and the
/// fd closes, so there is no unlock path to forget. A guard holding `None`
/// (locking disabled, or the lock could not be opened/acquired) is inert and
/// safe — this type is DEFAULT-OPEN: a broken lock must never be able to
/// suppress an alert.
pub struct InjectLock {
    _file: Option<std::fs::File>,
}

impl InjectLock {
    /// Acquire the shared inject lock, waiting up to [`ACQUIRE_TIMEOUT`].
    ///
    /// `reason` is a short label for the log line (e.g. `"cli"`, `"daemon"`),
    /// so a contended pane is diagnosable from the daemon log.
    pub async fn acquire(reason: &str) -> Self {
        let Some(path) = lock_path() else {
            debug!(reason, "inject lock disabled via {}=\"\"", INJECT_LOCK_ENV);
            return Self { _file: None };
        };

        // Best-effort parent creation: on a host where /var/run/claude is
        // provisioned by tmpfiles.d this is a no-op, but a fresh container or
        // a test sandbox may not have it yet.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                // DEFAULT-OPEN: proceed unserialized rather than drop the
                // inject. Loud, because it means the interleave guard is off.
                warn!(
                    reason,
                    path = %path.display(),
                    error = %e,
                    "inject lock: cannot open lock file; proceeding WITHOUT inject serialization"
                );
                return Self { _file: None };
            }
        };

        let fd = file.as_raw_fd();
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        let mut waited = false;

        loop {
            // Non-blocking flock + async sleep rather than a blocking flock:
            // this runs on the tokio runtime and must not park a worker
            // thread for up to 90s.
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                if waited {
                    debug!(reason, path = %path.display(), "inject lock: acquired after waiting");
                }
                return Self { _file: Some(file) };
            }

            let err = std::io::Error::last_os_error();
            let contended = matches!(
                err.raw_os_error(),
                Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
            );
            if !contended {
                warn!(
                    reason,
                    path = %path.display(),
                    error = %err,
                    "inject lock: flock failed; proceeding WITHOUT inject serialization"
                );
                return Self { _file: None };
            }

            if Instant::now() >= deadline {
                warn!(
                    reason,
                    path = %path.display(),
                    timeout_secs = ACQUIRE_TIMEOUT.as_secs(),
                    "inject lock: timed out waiting for a peer inject; proceeding ANYWAY \
                     (payloads may interleave on the prompt line)"
                );
                return Self { _file: None };
            }

            if !waited {
                waited = true;
                debug!(
                    reason,
                    path = %path.display(),
                    "inject lock: contended, another injector is typing; waiting"
                );
            }
            sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env mutation across tests in this module — `std::env::set_var`
    /// is process-global and these tests run on threads in one binary.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn empty_env_disables_locking() {
        let _g = ENV_GUARD.lock().unwrap();
        std::env::set_var(INJECT_LOCK_ENV, "");
        assert!(lock_path().is_none());
        std::env::remove_var(INJECT_LOCK_ENV);
    }

    #[test]
    fn unset_env_uses_absolute_default() {
        let _g = ENV_GUARD.lock().unwrap();
        std::env::remove_var(INJECT_LOCK_ENV);
        let p = lock_path().expect("locking enabled by default");
        assert_eq!(p, PathBuf::from(DEFAULT_INJECT_LOCK));
        // The whole point: env-INDEPENDENT. A systemd system unit has no
        // XDG_RUNTIME_DIR, so a $XDG_RUNTIME_DIR-relative default would
        // resolve to a DIFFERENT file for the daemon than for a login-shell
        // caller, and the serialization would silently not exist.
        assert!(p.is_absolute());
        assert!(!p.to_string_lossy().contains("run/user"));
    }

    #[test]
    fn env_override_is_honoured() {
        let _g = ENV_GUARD.lock().unwrap();
        std::env::set_var(INJECT_LOCK_ENV, "/tmp/cw-test-inject.lock");
        assert_eq!(
            lock_path().unwrap(),
            PathBuf::from("/tmp/cw-test-inject.lock")
        );
        std::env::remove_var(INJECT_LOCK_ENV);
    }

    /// Two guards over the same path must NOT be held simultaneously: the
    /// second waits. We assert the mutual exclusion by timing — the second
    /// acquire only returns after the first is dropped.
    #[tokio::test]
    async fn second_acquire_waits_for_the_first() {
        let dir = std::env::temp_dir().join(format!("cw-inject-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inject.lock");
        std::env::set_var(INJECT_LOCK_ENV, &path);

        let first = InjectLock::acquire("test-first").await;

        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let _second = InjectLock::acquire("test-second").await;
            started.elapsed()
        });

        // Hold the lock long enough that the waiter must observe contention.
        tokio::time::sleep(Duration::from_millis(600)).await;
        drop(first);

        let waited = handle.await.unwrap();
        assert!(
            waited >= Duration::from_millis(400),
            "second acquire returned in {waited:?} — it did not wait for the first"
        );

        std::env::remove_var(INJECT_LOCK_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
