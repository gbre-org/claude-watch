//! Discovers Claude Code's own "thinking verb" word list (the spinner
//! vocabulary it randomly cycles through while generating, e.g.
//! "Coalescing…", "Percolating…") directly from the locally installed
//! `claude` binary, instead of hardcoding an ever-growing, ever-stale list
//! of individual busy-indicator phrases in `status::pane_shows_active_ui`.
//!
//! Background: `pane_shows_active_ui` exists to stop the dead-process /
//! fresh-session-kick-start gate from firing into a genuinely active
//! session (recurring false positive, "operator #5620", 2026-08-27
//! regression, and again 2026-09-24 on the then-unrecognized "Coalescing…"
//! line). Each fix so far has been reactive — add the one new phrase that
//! misfired. Claude Code's actual spinner vocabulary is ~180 whimsical
//! present-participle verbs embedded as a single contiguous run of string
//! literals in its binary, immediately adjacent to the `spinnerVerbs`
//! symbol name left behind by its bundler. Extracting that block directly
//! means claude-watch tracks Claude Code's word list as it changes, rather
//! than chasing it one regression at a time.

use std::path::{Path, PathBuf};

/// Minimum run length for a printable-ASCII byte run to be considered a
/// candidate string — mirrors the Unix `strings` utility's default.
const MIN_STRING_LEN: usize = 4;

/// The distinctive symbol name Claude Code's bundler leaves adjacent to the
/// verb-list string literal. Used purely as a byte-offset anchor — we never
/// need to parse the surrounding bytecode, just find this marker and walk
/// backward through the preceding printable strings.
const ANCHOR: &str = "spinnerVerbs";

/// Pure: scan raw bytes for printable-ASCII runs >= `min_len`, in file order.
fn extract_printable_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    for &b in bytes {
        if (0x20..=0x7e).contains(&b) {
            current.push(b);
        } else {
            if current.len() >= min_len {
                out.push(String::from_utf8_lossy(&current).into_owned());
            }
            current.clear();
        }
    }
    if current.len() >= min_len {
        out.push(String::from_utf8_lossy(&current).into_owned());
    }
    out
}

/// Pure: does `s` look like one of Claude Code's spinner-verb words — a
/// single capitalized token of letters/apostrophes/hyphens?
fn looks_like_spinner_verb(s: &str) -> bool {
    let starts_uppercase = matches!(s.chars().next(), Some(c) if c.is_ascii_uppercase());
    starts_uppercase
        && (2..=30).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphabetic() || c == '\'' || c == '-')
}

/// Pure: given the ordered strings extracted from the binary, find the
/// contiguous run of spinner-verb-shaped tokens immediately preceding the
/// `spinnerVerbs` anchor and return them (in their original, file order).
///
/// Contiguous is the load-bearing word: the verb list sits as one unbroken
/// run of quoted string literals in the source array (`["Actualizing",
/// "Actioning", ..., "Zigzagging"]`); the moment a non-matching string is
/// hit walking backward from the anchor, we've walked past the start of the
/// array into unrelated bundle content.
fn spinner_verbs_from_strings(strings: &[String]) -> Vec<String> {
    let Some(anchor_idx) = strings.iter().position(|s| s == ANCHOR) else {
        return Vec::new();
    };
    let mut verbs = Vec::new();
    let mut i = anchor_idx;
    while i > 0 {
        i -= 1;
        if looks_like_spinner_verb(&strings[i]) {
            verbs.push(strings[i].clone());
        } else {
            break;
        }
    }
    verbs.reverse();
    verbs
}

/// Scan `path` (the local Claude Code binary) for its embedded spinner-verb
/// list. Returns `None` if the file can't be read or the anchor isn't found
/// (e.g. a future bundler change moves/renames the symbol) — callers must
/// treat that as "no extra verbs available", not an error; the existing
/// static markers in `status::pane_shows_active_ui` are the fallback.
pub fn discover_thinking_verbs(path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    let strings = extract_printable_strings(&bytes, MIN_STRING_LEN);
    let verbs = spinner_verbs_from_strings(&strings);
    if verbs.is_empty() {
        None
    } else {
        Some(verbs)
    }
}

/// Resolve the local `claude` binary path by walking `PATH` and following
/// symlinks to the real (versioned) executable — mirrors what `which claude`
/// followed by `readlink -f` would do, without shelling out to either.
pub fn resolve_claude_binary_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("claude");
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate).ok();
        }
    }
    None
}

static DISCOVERED_VERBS: tokio::sync::OnceCell<Vec<String>> = tokio::sync::OnceCell::const_new();

/// Cached, process-lifetime discovery: resolves the local `claude` binary
/// and extracts its spinner-verb list once, reusing the result for the rest
/// of the daemon's run. The scan reads the whole (200MB+) binary, so it runs
/// via `spawn_blocking` and is only ever paid once per process.
pub async fn discovered_thinking_verbs() -> &'static [String] {
    DISCOVERED_VERBS
        .get_or_init(|| async {
            tokio::task::spawn_blocking(|| {
                resolve_claude_binary_path()
                    .and_then(|path| discover_thinking_verbs(&path))
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        })
        .await
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_printable_runs_and_drops_short_ones() {
        let bytes = b"\x00\x01Hello\x00ab\x00World!\x02";
        let strings = extract_printable_strings(bytes, 4);
        assert_eq!(strings, vec!["Hello".to_string(), "World!".to_string()]);
    }

    #[test]
    fn spinner_verb_shape_accepts_gerunds_and_rejects_junk() {
        assert!(looks_like_spinner_verb("Coalescing"));
        assert!(looks_like_spinner_verb("Dilly-dallying"));
        assert!(looks_like_spinner_verb("Beboppin'"));
        assert!(!looks_like_spinner_verb("coalescing")); // lowercase start
        assert!(!looks_like_spinner_verb("Do you want to proceed?")); // spaces/punct
        assert!(!looks_like_spinner_verb("A")); // too short
    }

    #[test]
    fn finds_contiguous_block_immediately_before_the_anchor() {
        let strings: Vec<String> = [
            "unrelated bundle noise",
            "Actualizing",
            "Coalescing",
            "Zigzagging",
            "spinnerVerbs",
            "verbs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let verbs = spinner_verbs_from_strings(&strings);
        assert_eq!(verbs, vec!["Actualizing", "Coalescing", "Zigzagging"]);
    }

    #[test]
    fn stops_at_the_first_non_matching_token_walking_backward() {
        let strings: Vec<String> = [
            "some other string entirely",
            "Actualizing",
            "Coalescing",
            "spinnerVerbs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let verbs = spinner_verbs_from_strings(&strings);
        assert_eq!(verbs, vec!["Actualizing", "Coalescing"]);
    }

    #[test]
    fn missing_anchor_yields_no_verbs() {
        let strings: Vec<String> = ["Actualizing", "Coalescing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(spinner_verbs_from_strings(&strings).is_empty());
    }

    #[test]
    fn real_binary_extraction_end_to_end_if_claude_is_installed() {
        // Not a hard requirement (CI may not have `claude` on PATH), but
        // exercises the real end-to-end path when it is.
        let Some(path) = resolve_claude_binary_path() else {
            return;
        };
        if let Some(verbs) = discover_thinking_verbs(&path) {
            assert!(
                verbs.len() > 20,
                "expected a large verb list, got {:?}",
                verbs
            );
            assert!(verbs.iter().any(|v| v == "Coalescing"));
        }
    }
}
