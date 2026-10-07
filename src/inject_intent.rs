//! Short-lived "I expect a confirmation menu on this pane" intents.
//!
//! `claude-watch inject --submit '/model <id>'` normally answers the `Switch
//! model?` confirmation itself (see `inject_menu`), but only while the inject
//! process is alive. When the target pane is BUSY, Claude Code queues the
//! submitted slash command and runs it at the end of the turn, so the dialog
//! can appear minutes after `inject` has exited. Nothing bounded by the
//! inject process can see that.
//!
//! So an inject that submits an allowlisted menu-producing command also drops
//! an INTENT file here: which pane, which menu kind, when it lapses. The
//! long-running daemon sweeps the intents every loop pass and, when the pane
//! shows that exact dialog while an unexpired intent exists, answers it with
//! the same detection and key logic the synchronous path uses, then deletes
//! the intent.
//!
//! The safety properties, in order of importance:
//!
//! * No live intent, no answer. A `/model` a human typed keeps its normal
//!   interactive dialog.
//! * An intent lapses (default 15 minutes) and is deleted on the next sweep,
//!   so a stale one can never wake up later and press a key into a
//!   conversation.
//! * Keys come only from `tmux::model_switch_answer_keys` on a FRESH frame
//!   that shows the dialog with a readable cursor; no digit is ever sent.
//! * Bounded attempts per intent.

use crate::tmux;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};

/// How long an intent stays live after the submit.
pub const INTENT_TTL_SECS: u64 = 15 * 60;

/// Most answer sequences the daemon will send for one intent.
pub const MAX_ATTEMPTS: u32 = 3;

/// Subdirectory (under the daemon's state directory) holding intent files.
pub const INTENT_SUBDIR: &str = "inject-intents";

/// The menu kind an intent expects. One entry today.
pub const KIND_MODEL_SWITCH: &str = "model-switch";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub pane: String,
    pub kind: String,
    /// What to answer. Only `"confirm"` (apply the switch) exists.
    pub answer: String,
    pub created_epoch: u64,
    pub expires_epoch: u64,
    #[serde(default)]
    pub attempts: u32,
}

impl Intent {
    pub fn model_switch(pane: &str, now: u64) -> Self {
        Intent {
            pane: pane.to_string(),
            kind: KIND_MODEL_SWITCH.to_string(),
            answer: "confirm".to_string(),
            created_epoch: now,
            expires_epoch: now + INTENT_TTL_SECS,
            attempts: 0,
        }
    }

    pub fn expired(&self, now: u64) -> bool {
        now >= self.expires_epoch
    }
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The intent directory for a daemon whose state file is `state_file`.
pub fn intent_dir_for(state_file: &str) -> Option<PathBuf> {
    Path::new(state_file)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(INTENT_SUBDIR))
}

fn file_for(dir: &Path, pane: &str) -> PathBuf {
    let safe: String = pane
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// Write `intent` (replacing any earlier one for the same pane), atomically.
pub fn record(dir: &Path, intent: &Intent) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = file_for(dir, &intent.pane);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(intent).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)
}

/// Delete the intent for `pane`, if any.
pub fn clear(dir: &Path, pane: &str) {
    let _ = std::fs::remove_file(file_for(dir, pane));
}

/// Is there an unexpired intent for `pane`?
#[allow(dead_code)] // used by tests and library callers; the bin copy has none
pub fn has_live(dir: &Path, pane: &str, now: u64) -> bool {
    load(&file_for(dir, pane)).is_some_and(|i| !i.expired(now))
}

fn load(path: &Path) -> Option<Intent> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// All live intents; deletes expired and unreadable files as it goes.
pub fn live_intents(dir: &Path, now: u64) -> Vec<Intent> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = vec![];
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load(&path) {
            Some(i) if !i.expired(now) => out.push(i),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out
}

/// What one sweep did for one intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepAction {
    /// No dialog on the pane yet; intent kept.
    Waiting,
    /// Dialog up but the cursor row is unreadable; nothing pressed.
    Unreadable,
    /// Keys sent and the dialog went away; intent cleared.
    Answered,
    /// Keys sent but the dialog is still up; intent kept (bounded retries).
    StillUp,
    /// Attempts exhausted; intent dropped.
    GaveUp,
}

/// Sweep the intents once. `capture` reads a pane; `send` types the answer
/// keys. Both are injected so the logic is testable without tmux.
pub async fn sweep<C, CF, S, SF>(
    dir: &Path,
    now: u64,
    capture: C,
    send: S,
) -> Vec<(Intent, SweepAction)>
where
    C: Fn(String) -> CF,
    CF: Future<Output = Option<String>>,
    S: Fn(String, &'static [&'static str]) -> SF,
    SF: Future<Output = ()>,
{
    let mut results = vec![];
    for mut intent in live_intents(dir, now) {
        if intent.kind != KIND_MODEL_SWITCH {
            // Unknown kind (newer writer): never act on what we cannot read.
            continue;
        }
        let Some(frame) = capture(intent.pane.clone()).await else {
            results.push((intent, SweepAction::Waiting));
            continue;
        };
        if !tmux::model_switch_dialog_visible(&frame) {
            if intent.attempts > 0 {
                // We pressed an answer earlier and the dialog is gone now
                // (it was just slow to repaint): the job is done.
                clear(dir, &intent.pane);
                results.push((intent, SweepAction::Answered));
            } else {
                results.push((intent, SweepAction::Waiting));
            }
            continue;
        }
        let Some(keys) = tmux::model_switch_answer_keys(&frame) else {
            results.push((intent, SweepAction::Unreadable));
            continue;
        };
        if intent.attempts >= MAX_ATTEMPTS {
            clear(dir, &intent.pane);
            results.push((intent, SweepAction::GaveUp));
            continue;
        }
        intent.attempts += 1;
        // Persist the attempt BEFORE pressing anything, so a crash mid-answer
        // cannot grant unlimited retries.
        let _ = record(dir, &intent);
        send(intent.pane.clone(), keys).await;
        let gone = match capture(intent.pane.clone()).await {
            Some(f) => !tmux::model_switch_dialog_visible(&f),
            None => false,
        };
        if gone {
            clear(dir, &intent.pane);
            results.push((intent, SweepAction::Answered));
        } else {
            results.push((intent, SweepAction::StillUp));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const DIALOG: &str = "  Switch model?\n  Your next response will be slower\n\n\
        ❯ 1. Yes, switch to Opus 5\n  2. No, go back\n";
    const DIALOG_DECLINE_SELECTED: &str = "  Switch model?\n\n\
        1. Yes, switch to Opus 5\n❯ 2. No, go back\n";
    const BUSY: &str = "● Working on things...\n✻ Thinking… (12s)\n";
    const IDLE: &str = "  Model set to opus\n❯ \n";

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Fake pane: serves queued frames (last one repeats) and logs keys sent.
    struct Fake {
        frames: Mutex<Vec<String>>,
        sent: Mutex<Vec<Vec<&'static str>>>,
    }
    impl Fake {
        fn new(frames: &[&str]) -> Self {
            Fake {
                frames: Mutex::new(frames.iter().rev().map(|s| s.to_string()).collect()),
                sent: Mutex::new(vec![]),
            }
        }
        fn capture(&self) -> Option<String> {
            let mut f = self.frames.lock().unwrap();
            if f.len() > 1 {
                f.pop()
            } else {
                f.last().cloned()
            }
        }
        fn push_sent(&self, keys: &'static [&'static str]) {
            self.sent.lock().unwrap().push(keys.to_vec());
        }
    }

    async fn run(dir: &Path, now: u64, fake: &Fake) -> Vec<(Intent, SweepAction)> {
        sweep(
            dir,
            now,
            |_p| {
                let r = fake.capture();
                async move { r }
            },
            |_p, k| {
                fake.push_sent(k);
                async {}
            },
        )
        .await
    }

    #[tokio::test]
    async fn deferred_menu_is_answered_when_it_appears() {
        let d = tmpdir();
        record(d.path(), &Intent::model_switch("dashboard:0.0", 1000)).unwrap();
        // First sweep: turn still running, no dialog. Intent survives.
        let busy = Fake::new(&[BUSY]);
        let r = run(d.path(), 1010, &busy).await;
        assert_eq!(r[0].1, SweepAction::Waiting);
        assert!(busy.sent.lock().unwrap().is_empty());
        assert!(has_live(d.path(), "dashboard:0.0", 1010));
        // Later: dialog appears; after Enter it is gone.
        let fake = Fake::new(&[DIALOG, IDLE]);
        let r = run(d.path(), 1200, &fake).await;
        assert_eq!(r[0].1, SweepAction::Answered);
        assert_eq!(*fake.sent.lock().unwrap(), vec![vec!["Enter"]]);
        assert!(!has_live(d.path(), "dashboard:0.0", 1200));
    }

    #[tokio::test]
    async fn decline_row_gets_up_then_enter() {
        let d = tmpdir();
        record(d.path(), &Intent::model_switch("p:0.0", 0)).unwrap();
        let fake = Fake::new(&[DIALOG_DECLINE_SELECTED, IDLE]);
        run(d.path(), 5, &fake).await;
        assert_eq!(*fake.sent.lock().unwrap(), vec![vec!["Up", "Enter"]]);
    }

    #[tokio::test]
    async fn no_intent_leaves_the_menu_alone() {
        let d = tmpdir();
        let fake = Fake::new(&[DIALOG]);
        let r = run(d.path(), 1000, &fake).await;
        assert!(r.is_empty());
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_intent_leaves_the_menu_alone_and_is_deleted() {
        let d = tmpdir();
        record(d.path(), &Intent::model_switch("dashboard:0.0", 1000)).unwrap();
        let fake = Fake::new(&[DIALOG]);
        let r = run(d.path(), 1000 + INTENT_TTL_SECS, &fake).await;
        assert!(r.is_empty());
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!file_for(d.path(), "dashboard:0.0").exists());
    }

    #[tokio::test]
    async fn only_the_intents_own_pane_is_inspected() {
        let d = tmpdir();
        record(d.path(), &Intent::model_switch("other:0.0", 0)).unwrap();
        let seen = Mutex::new(vec![]);
        sweep(
            d.path(),
            1,
            |p| {
                seen.lock().unwrap().push(p);
                async { Some(BUSY.to_string()) }
            },
            |_p, _k| async {},
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), vec!["other:0.0".to_string()]);
    }

    #[tokio::test]
    async fn attempts_are_bounded() {
        let d = tmpdir();
        record(d.path(), &Intent::model_switch("p:0.0", 0)).unwrap();
        let fake = Fake::new(&[DIALOG]); // never closes
        for _ in 0..MAX_ATTEMPTS {
            let r = run(d.path(), 1, &fake).await;
            assert_eq!(r[0].1, SweepAction::StillUp);
        }
        let r = run(d.path(), 1, &fake).await;
        assert_eq!(r[0].1, SweepAction::GaveUp);
        assert_eq!(fake.sent.lock().unwrap().len(), MAX_ATTEMPTS as usize);
        assert!(!has_live(d.path(), "p:0.0", 1));
    }

    #[test]
    fn record_replaces_and_clear_removes() {
        let d = tmpdir();
        record(d.path(), &Intent::model_switch("p:0.0", 10)).unwrap();
        record(d.path(), &Intent::model_switch("p:0.0", 20)).unwrap();
        assert_eq!(live_intents(d.path(), 21).len(), 1);
        clear(d.path(), "p:0.0");
        assert!(live_intents(d.path(), 21).is_empty());
    }

    #[test]
    fn intent_dir_sits_beside_the_state_file() {
        assert_eq!(
            intent_dir_for("/var/lib/cw/state.json").unwrap(),
            PathBuf::from("/var/lib/cw/inject-intents")
        );
        assert!(intent_dir_for("state.json").is_none());
    }
}
