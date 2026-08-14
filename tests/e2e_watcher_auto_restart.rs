//! End-to-end test for the watcher-down auto-restart-via-tmux-inject path.
//!
//! Marked `#[ignore]` because it spins up a real tmux session, spawns child
//! processes, and depends on `tmux` + `pgrep` + `bash` being on PATH. Run
//! manually before merging changes that touch the watcher-monitor or the
//! tmux-inject pipeline:
//!
//! ```
//! cargo test --test e2e_watcher_auto_restart -- --ignored --nocapture
//! ```
//!
//! ## What it verifies
//!
//! 1. **DOWN detection** — the daemon notices the watcher process is
//!    missing within `inject_threshold * check_interval` seconds.
//! 2. **Inject delivery** — the daemon's `tmux send-keys` sequence is
//!    delivered into the test pane (visible via `tmux capture-pane` and
//!    captured to a side log when the stub bash reads it from stdin).
//! 3. **Stub main-loop reaction** — the in-pane stub bash reads the
//!    inject string from its stdin (the pane TTY) and exec's
//!    `watcher-ctl run <name>`, which restarts the watcher.
//! 4. **Parent-chain liveness invariant** — the restarted watcher's PPID
//!    chain ends at the test tmux session, NOT at systemd / claude-watch.
//!    This is the heartbeat-liveness contract from PR #44: if the main
//!    loop dies, the watcher must die too.
//! 5. **Heartbeat liveness** — when the test tmux session is killed, the
//!    watcher dies with it.
//!
//! ## Architecture
//!
//! The test simulates the production wiring without touching the live
//! system:
//!
//! - **Test tmux session** (unique name per test run): hosts a bash
//!   "stub main loop" that displays a fake idle Claude Code TUI and
//!   reads keystrokes from its pane TTY (stdin), forwarding any
//!   `watcher-ctl run <name>` line into actual exec — simulating the
//!   real main-loop's Claude Code process executing the inject as a
//!   background Bash tool call.
//! - **Mock `watcher-ctl run`** on PATH: identical to the real one for
//!   the purposes of this test — it exec's the watcher's `start_cmd`
//!   from the test watchers.conf.
//! - **Synthetic test watcher**: a `sleep 99999` carrying a unique
//!   marker string (`cw-test-watcher-<uuid>`) so `pgrep -fc -- <pattern>`
//!   matches only this test's watcher, never the live system's.
//! - **claude-watch daemon**: spawned with a custom config that points
//!   at the test pane, the test watchers.conf, and uses fast intervals
//!   (`check_interval=1`, `inject_threshold=2`, `grace_secs=0`,
//!   `inject_cooldown=2`) so the whole inject path completes in <30s.

mod common;

use common::{TestEnv, TestEnvOptions};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_token(prefix: &str) -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}-{}-{}", prefix, std::process::id(), n)
}

/// Path to the daemon binary under test.
///
/// Cargo has already built it and handed us the path in
/// `CARGO_BIN_EXE_<name>`. See `tests/common/mod.rs::daemon_binary` for why
/// invoking `cargo build` from inside a test is not an option.
fn daemon_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_claude-watch"))
}

/// Count processes matching a pgrep pattern.
fn pgrep_count(pattern: &str) -> u32 {
    let out = Command::new("pgrep")
        .args(["-fc", "--", pattern])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Get the list of PIDs matching a pgrep pattern.
fn pgrep_pids(pattern: &str) -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-f", "--", pattern])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Walk PPID chain starting from `pid`, returning each (pid, comm) tuple
/// up to PID 1. Used to assert the watcher is parented to the test tmux
/// session, not systemd.
fn ppid_chain(pid: u32) -> Vec<(u32, String)> {
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..32 {
        if current == 0 || current == 1 {
            // Include PID 1 in the chain so callers can detect the chain
            // terminating at init / systemd
            if current == 1 {
                let comm = fs::read_to_string("/proc/1/comm")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                chain.push((1, comm));
            }
            break;
        }
        let stat_path = format!("/proc/{}/stat", current);
        let stat = match fs::read_to_string(&stat_path) {
            Ok(s) => s,
            Err(_) => break,
        };
        // /proc/PID/stat: pid (comm) state ppid ...
        // comm may contain spaces; it's wrapped in parens.
        let close = match stat.rfind(')') {
            Some(i) => i,
            None => break,
        };
        let after = &stat[close + 2..];
        let mut fields = after.split_whitespace();
        let _state = fields.next();
        let ppid: u32 = match fields.next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => break,
        };
        let comm_start = stat.find('(').unwrap_or(0) + 1;
        let comm = stat[comm_start..close].to_string();
        chain.push((current, comm));
        current = ppid;
    }
    chain
}

/// Capture text from the tmux pane. Returns "" on failure.
fn capture_pane(pane: &str) -> String {
    let out = Command::new("tmux")
        .args(["capture-pane", "-t", pane, "-p", "-S", "-200"])
        .output()
        .expect("tmux capture-pane");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Block until `predicate()` returns true, polling every 200ms. Returns
/// false if the deadline expires first.
fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    predicate()
}

/// End-to-end watcher-down auto-restart via real tmux + real claude-watch
/// daemon. See module docstring for the full setup.
#[test]
#[ignore]
fn watcher_down_triggers_inject_and_main_loop_restarts_it() {
    // 1. Set up the test environment. We delegate config + temp dirs +
    //    mock claude-status to the common harness, but we'll bring our
    //    own tmux session (the harness's default empty session can't host
    //    our stub main loop bash).
    let watcher_token = unique_token("cw-test-watcher");
    let env = TestEnv::new(
        "watcher-restart",
        TestEnvOptions {
            check_interval: 1,
            heartbeat_stale_minutes: 9999, // disable heartbeat alerts
            foreground_threshold: 9999,    // disable foreground monitor
            // Watcher monitor: fast firing path.
            //   threshold=2 -> two consecutive missing checks -> ~2s
            //   grace=0     -> no warm-up after last_seen_running
            //   cooldown=2  -> immediate re-fire if first inject was lost
            watcher_inject_threshold: 2,
            watcher_inject_cooldown: 2,
            watcher_grace_secs: 0,
            // We'll create the tmux session manually with our stub main loop.
            skip_tmux_session: true,
            ..Default::default()
        },
    );

    // 2. Write a watchers.conf entry pointing at our synthetic watcher.
    //    The pattern is the unique token so pgrep only matches this test.
    let watcher_start_cmd = format!("sleep_with_marker {}", watcher_token);
    fs::write(
        &env.watchers_config,
        format!(
            "{name}|{pattern}|1|true|{cmd}\n",
            name = watcher_token,
            pattern = watcher_token,
            cmd = watcher_start_cmd,
        ),
    )
    .expect("write watchers.conf");

    // 3. Mock `sleep_with_marker` and `watcher-ctl` on PATH.
    //
    //    `sleep_with_marker <token>` -> exec sleep 99999 with the token
    //    in argv so pgrep -f matches it.
    //
    //    `watcher-ctl run <name>` -> read watchers.conf, find the entry,
    //    exec start_cmd. This mirrors the real watcher-ctl's behavior for
    //    the purposes of this test.
    let sleep_marker_path = env.mock_bin_dir.join("sleep_with_marker");
    fs::write(
        &sleep_marker_path,
        r#"#!/bin/bash
# Identifiable long-running process. We set argv[0] to "<marker>-sleep" via
# `exec -a` so pgrep -f matches the marker token, but sleep itself only
# sees the numeric duration in argv[1] (sleep is strict about its args).
marker="$1"
exec -a "${marker}-sleep" sleep 99999
"#,
    )
    .expect("write sleep_with_marker");
    chmod_exec(&sleep_marker_path);

    let watcher_ctl_path = env.mock_bin_dir.join("watcher-ctl");
    fs::write(
        &watcher_ctl_path,
        format!(
            r#"#!/bin/bash
# Mock watcher-ctl for the e2e test. Only handles `run <name>`. Looks up
# the matching entry in WATCHERS_CONFIG and exec's its start_cmd.
set -e
if [ "$1" != "run" ]; then
    exit 0
fi
name="$2"
config="{config}"
line=$(grep "^${{name}}|" "$config" || true)
if [ -z "$line" ]; then
    echo "watcher-ctl: $name not found in $config" >&2
    exit 1
fi
# field 5 = start_cmd
cmd=$(echo "$line" | awk -F'|' '{{print $5}}')
exec $cmd
"#,
            config = env.watchers_config.display()
        ),
    )
    .expect("write watcher-ctl");
    chmod_exec(&watcher_ctl_path);

    // 4. Build the stub-main-loop script. This is the bash that runs
    //    INSIDE the test tmux pane. It:
    //      - Renders a fake idle Claude Code TUI so claude-watch's
    //        get_activity() returns Idle and inject_text proceeds.
    //      - Polls its own pane via `tmux capture-pane` for the inject
    //        string, exec'ing `watcher-ctl run <name>` in the background
    //        when found (so the watcher is parented to THIS bash, which
    //        is parented to the test tmux session).
    //      - Stays alive forever (until SIGTERM from session teardown).
    let stub_log = env.tmp_dir.join("stub-main-loop.log");
    let stub_path = env.tmp_dir.join("stub-main-loop.sh");
    // Use the same session/pane name the harness wrote into our daemon's
    // [tmux] config — the daemon's `dashboard_pane` points there. The
    // harness was configured with `skip_tmux_session=true` so we own
    // creating it.
    let stub_session = env.tmux_session.clone();
    let stub_pane = env.tmux_pane.clone();
    // PATH for the stub bash and the watcher-ctl invocation inside it. We
    // must compute this BEFORE writing the stub script so the export PATH
    // statement embedded in the stub knows where the mock binaries live.
    let test_path = env.test_path();
    fs::write(
        &stub_path,
        format!(
            r#"#!/bin/bash
# Stub main loop for the e2e watcher-restart test.
# Renders an idle Claude TUI layout, then reads from stdin (the tmux
# pane's TTY) so keystrokes sent via `tmux send-keys` actually echo /
# accumulate. Periodically scans recent stdin lines for the inject
# string and exec's `watcher-ctl run <name>` when one matches.

set +e

# tmux new-session ignores the env passed to its CLI invocation when
# inheriting from the server, so set PATH + WATCHERS_CONFIG explicitly
# here. This is the env that the spawned watcher-ctl child inherits.
export PATH="{test_path}"
export WATCHERS_CONFIG="{watchers_config}"

log="{log}"
keys_log="{keys_log}"
seen=""

# Fake Claude TUI in idle state. The daemon's detect_activity() looks for:
#   - a separator line (U+2500 repeated)
#   - the prompt char (U+276F = ❯) below the separator
#   - "-- INSERT --" status bar at the bottom
# Anything above the separator that's NOT a thinking indicator counts as
# Idle.
clear
printf '\xE2\x9C\xBB Brewed for 12s \xC2\xB7 64 tokens (test stub)\n'
printf '\n'
printf '%.0s\xE2\x94\x80' {{1..70}}; printf '\n'
printf '\xE2\x9D\xAF \n'
printf '%.0s\xE2\x94\x80' {{1..70}}; printf '\n'
printf '  -- INSERT -- 50000 tokens\n'

# Watcher loop: scan the keystroke log for inject signatures every second
# and exec watcher-ctl run when one matches.
(
    while true; do
        sleep 1
        if [ -f "$keys_log" ]; then
            if grep -q "watcher-ctl run {token}" "$keys_log" 2>/dev/null; then
                if [ "$seen" != "fired" ]; then
                    seen="fired"
                    line=$(grep "watcher-ctl run {token}" "$keys_log" | head -1)
                    echo "$(date -Is) STUB SAW INJECT: $line" >> "$log"
                    watcher-ctl run {token} >> "$log" 2>&1 &
                    echo "$(date -Is) STUB STARTED watcher pid=$!" >> "$log"
                fi
            fi
        fi
    done
) &

# Read stdin line-by-line and append to the keystroke log. Each `tmux
# send-keys` Enter delivers one line into our stdin; `dd` / `i` from
# inject_text become part of the line text. Bash readline (interactive)
# would consume Escape sequences; we run with `read -r` in non-interactive
# mode which sees raw bytes.
while IFS= read -r raw_line; do
    printf '%s\n' "$raw_line" >> "$keys_log"
done

# If stdin closes (tmux teardown), block forever so the pane stays alive
# until the session is killed.
sleep infinity
"#,
            log = stub_log.display(),
            keys_log = env.tmp_dir.join("stub-keys.log").display(),
            token = watcher_token,
            test_path = test_path,
            watchers_config = env.watchers_config.display(),
        ),
    )
    .expect("write stub script");
    chmod_exec(&stub_path);

    // 5. Spawn the test tmux session running the stub script.

    // Kill any leftover from a previous failed run.
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &stub_session])
        .output();

    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &stub_session,
            "-x",
            "200",
            "-y",
            "50",
            "bash",
            stub_path.to_str().unwrap(),
        ])
        .env("PATH", &test_path)
        .env(
            "WATCHERS_CONFIG",
            env.watchers_config.to_str().unwrap(),
        )
        .status()
        .expect("create stub tmux session");
    assert!(status.success(), "tmux new-session failed");

    // RAII: kill the stub tmux session and any spawned watcher on drop.
    let _guard = TmuxSessionGuard {
        session: stub_session.clone(),
        watcher_token: watcher_token.clone(),
    };

    // Give the stub a moment to render its fake idle TUI.
    std::thread::sleep(Duration::from_millis(800));
    let initial = capture_pane(&stub_pane);
    eprintln!(
        "INITIAL PANE CONTENT:\n--- begin ---\n{}\n--- end ---",
        initial
    );
    assert!(
        initial.contains('\u{276F}'),
        "stub did not render the prompt char (U+276F). Pane content:\n{}",
        initial
    );

    // 6. Spawn the synthetic watcher in the stub bash so its parent
    //    chain mirrors production. We exec it via `watcher-ctl run` from
    //    the same pane — which means we tmux-send-keys the command into
    //    the running stub script's stdin... except the stub is a `while
    //    true` loop, not a shell. Easiest: spawn directly via
    //    `tmux respawn-pane` is too disruptive. Use `tmux send-keys` to
    //    a fresh sub-shell? Cleaner: just spawn the watcher as a child
    //    of the SAME tmux session via `tmux new-window`.
    //
    //    Production layout: watcher -> watcher-ctl -> bash (Claude Code
    //    Bash tool) -> claude (main loop) -> tmux pane -> tmux server.
    //    For our test: spawn the watcher in a second window of the same
    //    tmux session, so it's still parented to the test tmux server.
    // Note: `tmux new-window` takes its `shell-command` as a SINGLE
    // string argument that the tmux server passes to /bin/sh -c. We
    // also set PATH + WATCHERS_CONFIG inline because the tmux server
    // ignores the env passed to the CLI invocation.
    let watcher_shell_cmd = format!(
        "PATH={path} WATCHERS_CONFIG={cfg} bash -c 'watcher-ctl run {token}'",
        path = test_path,
        cfg = env.watchers_config.display(),
        token = watcher_token,
    );
    let initial_watcher_status = Command::new("tmux")
        .args([
            "new-window",
            "-t",
            &stub_session,
            "-d",
            "-n",
            "watcher",
            &watcher_shell_cmd,
        ])
        .status()
        .expect("spawn initial watcher window");
    assert!(initial_watcher_status.success(), "tmux new-window failed");

    // Wait for the watcher to be visible to pgrep.
    let watcher_visible = wait_until(Duration::from_secs(5), || {
        pgrep_count(&watcher_token) >= 1
    });
    assert!(
        watcher_visible,
        "synthetic watcher did not start (pgrep -fc -- {} == 0)",
        watcher_token
    );
    let initial_pids = pgrep_pids(&watcher_token);
    eprintln!("initial watcher PIDs: {:?}", initial_pids);
    assert_eq!(initial_pids.len(), 1, "expected exactly one initial watcher");
    let initial_pid = initial_pids[0];

    // Verify the parent chain ends at a tmux process under the test session.
    let chain = ppid_chain(initial_pid);
    eprintln!("initial watcher PPID chain: {:?}", chain);
    let chain_has_tmux = chain.iter().any(|(_, comm)| comm.contains("tmux"));
    assert!(
        chain_has_tmux,
        "initial watcher PPID chain should include tmux (heartbeat-liveness invariant). Chain: {:?}",
        chain
    );

    // 7. Spawn the daemon. The harness's run_daemon_cycles wraps in a
    //    fixed wait then SIGTERM, but for this test we want to control
    //    the lifetime so we can synchronize on inject events. So spawn
    //    directly.
    let binary = daemon_binary();
    let daemon = Command::new(&binary)
        .env("CLAUDE_WATCH_CONFIG", &env.config_path)
        .env("PATH", &test_path)
        .env("CLAUDE_STATUS_CMD", "1")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let daemon_pid = daemon.id();
    eprintln!("daemon PID: {}", daemon_pid);

    // Make sure we tear the daemon down even on assertion failure.
    let _daemon_guard = DaemonGuard { pid: daemon_pid };

    // 8. Give the daemon a check cycle to record last_seen_running for
    //    the watcher.
    let saw_alive = wait_until(Duration::from_secs(8), || {
        let state = env.read_state();
        state["watcher_health"][&watcher_token]["last_seen_running"].is_string()
    });
    assert!(
        saw_alive,
        "daemon did not record last_seen_running. State: {}",
        env.read_state()
    );

    // 9. Kill the watcher.
    eprintln!("killing initial watcher pid={}", initial_pid);
    let kill_status = Command::new("kill")
        .arg(initial_pid.to_string())
        .status()
        .expect("kill watcher");
    assert!(kill_status.success() || pgrep_count(&watcher_token) == 0);

    let dead = wait_until(Duration::from_secs(3), || pgrep_count(&watcher_token) == 0);
    assert!(dead, "watcher did not die after kill");
    let kill_time = Instant::now();

    // 10. Wait for the daemon to (a) detect DOWN, (b) inject, and the
    //     stub to (c) restart the watcher. Total budget: 30s.
    //
    //     The inject is delivered via `tmux send-keys` which feeds the
    //     stub's stdin. The stub appends each line to keys_log; we
    //     check there for the "watcher-ctl run <token>" signature.
    let keys_log = env.tmp_dir.join("stub-keys.log");
    let inject_landed = wait_until(Duration::from_secs(20), || {
        let content = fs::read_to_string(&keys_log).unwrap_or_default();
        content.contains("WATCHER(S) DOWN") && content.contains(&watcher_token)
    });
    let pane_at_inject = capture_pane(&stub_pane);
    let keys_at_inject = fs::read_to_string(&keys_log).unwrap_or_default();
    eprintln!(
        "PANE AFTER INJECT WAIT:\n--- begin ---\n{}\n--- end ---",
        pane_at_inject
    );
    eprintln!(
        "KEYS LOG AFTER INJECT WAIT:\n--- begin ---\n{}\n--- end ---",
        keys_at_inject
    );
    if !inject_landed {
        eprintln!("DAEMON LOG:\n{}", env.read_legacy_log());
        eprintln!("DAEMON JSONL EVENTS:\n{:#?}", env.read_log_entries());
        eprintln!("DAEMON STATE:\n{}", env.read_state());
        eprintln!(
            "STUB LOG:\n{}",
            fs::read_to_string(&stub_log).unwrap_or_default()
        );
        // Show what the stub session is doing
        let panes_out = Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                &stub_session,
                "-F",
                "#{pane_id} #{pane_pid} #{pane_current_command}",
            ])
            .output();
        if let Ok(p) = panes_out {
            eprintln!("STUB PANES:\n{}", String::from_utf8_lossy(&p.stdout));
        }
    }
    assert!(
        inject_landed,
        "daemon did not deliver WATCHER(S) DOWN inject within 20s. \
         keys_log:\n{}",
        keys_at_inject
    );

    // Verify the daemon logged a watcher_inject event.
    let events = env.find_log_events("watcher_inject");
    assert!(
        !events.is_empty(),
        "daemon did not log watcher_inject event. JSONL log:\n{:?}",
        env.read_log_entries()
    );

    // Wait for the stub to react and the watcher to come back.
    let recovered = wait_until(Duration::from_secs(10), || {
        pgrep_count(&watcher_token) >= 1
    });
    let total_recovery = kill_time.elapsed();
    eprintln!("recovery elapsed: {:?}", total_recovery);
    assert!(
        recovered,
        "watcher did not come back after inject. Stub log:\n{}\nPane:\n{}",
        fs::read_to_string(&stub_log).unwrap_or_default(),
        capture_pane(&stub_pane)
    );
    assert!(
        total_recovery < Duration::from_secs(30),
        "recovery took too long: {:?}",
        total_recovery
    );

    let new_pids = pgrep_pids(&watcher_token);
    assert_eq!(
        new_pids.len(),
        1,
        "expected exactly one restarted watcher. PIDs: {:?}",
        new_pids
    );
    let new_pid = new_pids[0];
    assert_ne!(
        new_pid, initial_pid,
        "restarted watcher has same PID as the killed one — pgrep timing bug?"
    );

    // 11. Parent chain: must include tmux, must NOT include systemd /
    //     claude-watch.service.
    let chain = ppid_chain(new_pid);
    eprintln!("restarted watcher PPID chain: {:?}", chain);
    let comms: Vec<&str> = chain.iter().map(|(_, c)| c.as_str()).collect();
    assert!(
        comms.iter().any(|c| c.contains("tmux")),
        "restarted watcher PPID chain MUST include tmux. Chain: {:?}",
        chain
    );
    assert!(
        !comms.iter().any(|c| *c == "claude-watch"),
        "restarted watcher PPID chain MUST NOT include claude-watch \
         (heartbeat-liveness invariant). Chain: {:?}",
        chain
    );
    // We expect chain to terminate at PID 1 = systemd, but no claude-watch.service
    // ancestor. Ensure tmux comes before PID 1 (i.e. tmux is an ancestor of the
    // watcher, and the watcher isn't directly parented to systemd).
    let tmux_idx = comms.iter().position(|c| c.contains("tmux"));
    let init_idx = comms
        .iter()
        .position(|c| *c == "systemd" || *c == "init");
    if let (Some(t), Some(i)) = (tmux_idx, init_idx) {
        assert!(
            t < i,
            "tmux must be an ancestor of the watcher BEFORE systemd. Chain: {:?}",
            chain
        );
    }

    // 12. Heartbeat liveness: kill the test tmux session, watcher dies.
    eprintln!("tearing down test tmux session to verify heartbeat liveness");
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &stub_session])
        .output();

    let watcher_dies_with_session = wait_until(Duration::from_secs(10), || {
        pgrep_count(&watcher_token) == 0
    });
    assert!(
        watcher_dies_with_session,
        "watcher survived tmux session teardown — heartbeat-liveness invariant violated. \
         Live PIDs: {:?}",
        pgrep_pids(&watcher_token)
    );

    // 13. Done. Daemon shutdown is handled by DaemonGuard::drop, tmux
    //     cleanup by TmuxSessionGuard::drop.
    eprintln!("test passed; total recovery {:?}", total_recovery);
}

/// Drop-guard that kills a tmux session and any leftover watcher
/// processes matching the test token.
struct TmuxSessionGuard {
    session: String,
    watcher_token: String,
}

impl Drop for TmuxSessionGuard {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.session])
            .output();
        // Kill any remaining watcher processes by token.
        let pids = pgrep_pids(&self.watcher_token);
        for pid in pids {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

/// Drop-guard that SIGTERMs the daemon and waits for it to exit.
struct DaemonGuard {
    pid: u32,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid as i32, libc::SIGTERM);
        }
        // Reap zombie if the test process still has it as a child. We
        // can't access the original Child handle, but kill should be
        // enough; the kernel will reap once the parent (the test
        // executable) exits.
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn chmod_exec(path: &PathBuf) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
}
