//! token_usage — aggregate Claude Code token usage from JSONL transcripts
//! into Prometheus textfile metrics.
//!
//! Claude Code records every assistant turn into a JSONL transcript under
//! `~/.claude/projects/<project-slug>/<session-uuid>.jsonl` (the main
//! session) and `~/.claude/projects/<…>/<session-uuid>/subagents/agent-*.jsonl`
//! (subagents). Each assistant line carries a `message.usage` block:
//!
//! ```json
//! {"type":"assistant","timestamp":"2026-06-27T05:01:16.669Z",
//!  "message":{"id":"msg_…","usage":{
//!    "input_tokens":7082,"output_tokens":107,
//!    "cache_creation_input_tokens":22315,"cache_read_input_tokens":15835}}}
//! ```
//!
//! This module re-uses the SAME observation surface the rest of cw already
//! reads (the projects/ JSONL transcripts — see `active_agents.rs`) and folds
//! the per-turn `usage` numbers into two derived series, emitted through the
//! EXISTING `claude-watch metrics` textfile collector (`metrics.rs`), NOT a
//! second exporter:
//!
//!   * `claude_code_tokens_total{type=…}` — an all-time cumulative COUNTER
//!     (sum over every retained transcript). Drives the per-day bar chart via
//!     `increase(claude_code_tokens_total[1d])`.
//!   * `claude_code_tokens_month_to_date{type=…}` — a GAUGE summing only the
//!     turns whose local timestamp falls in the current calendar month. Resets
//!     to zero naturally on the 1st (no transcript timestamps in the new month
//!     yet), so a month-to-date stat panel needs no calendar-aware PromQL.
//!
//! ## Two correctness details proven against real on-disk transcripts
//!
//!  1. **Within-file duplicate turns.** Claude Code writes the SAME assistant
//!     message (identical `message.id` AND identical `usage`) to the transcript
//!     more than once (streaming partial + final). Empirically ~half of the
//!     usage-bearing lines are dupes. We dedup by `message.id` per file and
//!     count each id once, or the totals roughly double.
//!  2. **No cross-file duplication.** A `message.id` was never observed in two
//!     different transcript files (resume continues the same session file). So
//!     per-file dedup is sufficient and per-file results are independent —
//!     which is what makes the incremental cache below correct.
//!
//! ## Incremental cache
//!
//! Re-parsing every transcript on each emission (the collector runs ~1×/min
//! from cron) is wasteful: hundreds of MB of immutable history. We cache each
//! file's per-day token sums keyed by `(size, mtime)`; an unchanged file is
//! reused verbatim, so steady-state cost is "parse the one growing file." The
//! cache lives next to the daemon state at
//! `~/.config/claude-watch/token-usage-cache.json` and is written atomically.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Token totals broken down by the four categories Claude Code reports in a
/// `usage` block. All four map to a `type="…"` label on the emitted series.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl TokenCounts {
    fn add_assign(&mut self, other: &TokenCounts) {
        self.input += other.input;
        self.output += other.output;
        self.cache_creation += other.cache_creation;
        self.cache_read += other.cache_read;
    }
}

/// The two derived views the dashboard consumes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    /// All-time cumulative totals across every retained transcript.
    pub cumulative: TokenCounts,
    /// Totals for the current local calendar month (month-to-date).
    pub month_to_date: TokenCounts,
}

/// Per-file cache entry: the stat signature plus the file's per-local-day
/// token sums. `days` keys are `YYYY-MM-DD` strings in local time.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct CacheEntry {
    size: u64,
    /// File mtime as whole seconds since the epoch. Whole seconds is plenty to
    /// detect a re-append (the size almost always changes too) and avoids
    /// platform sub-second mtime-resolution flakiness.
    mtime: i64,
    days: HashMap<String, TokenCounts>,
}

/// On-disk cache shape: a map of absolute transcript path → entry.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    files: HashMap<String, CacheEntry>,
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()))
}

fn default_projects_dir() -> PathBuf {
    home_dir().join(".claude/projects")
}

fn default_cache_path() -> PathBuf {
    home_dir().join(".config/claude-watch/token-usage-cache.json")
}

/// Pull a `u64` token field out of a `usage` JSON object, defaulting to 0.
fn usage_field(usage: &serde_json::Value, key: &str) -> u64 {
    usage
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))
        .unwrap_or(0)
}

/// Convert an ISO-8601 transcript timestamp to a local `YYYY-MM-DD` day key.
/// Returns `None` if the timestamp is missing/unparseable so the turn is
/// skipped rather than mis-bucketed.
fn local_day_key(ts: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(ts.trim()).ok()?;
    Some(dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

/// Parse one transcript's content into per-local-day token sums.
///
/// Dedups by `message.id` within this file (see module docs): the first line
/// carrying a given id is counted, later repeats of the same id are ignored.
/// Pure: no I/O, no clock — fully testable.
pub fn parse_transcript_days(content: &str) -> HashMap<String, TokenCounts> {
    let mut days: HashMap<String, TokenCounts> = HashMap::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for line in content.lines() {
        // Cheap pre-filter: only assistant turns carry a usage block.
        if !line.contains("\"usage\"") {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let message = match obj.get("message") {
            Some(m) => m,
            None => continue,
        };
        let usage = match message.get("usage") {
            Some(u) => u,
            None => continue,
        };

        // Dedup by message id (streaming partial + final write the same id).
        // Turns without an id are singletons and always counted.
        if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
            if !seen_ids.insert(id.to_string()) {
                continue;
            }
        }

        let day = match obj.get("timestamp").and_then(|v| v.as_str()) {
            Some(ts) => match local_day_key(ts) {
                Some(d) => d,
                None => continue,
            },
            None => continue,
        };

        let counts = TokenCounts {
            input: usage_field(usage, "input_tokens"),
            output: usage_field(usage, "output_tokens"),
            cache_creation: usage_field(usage, "cache_creation_input_tokens"),
            cache_read: usage_field(usage, "cache_read_input_tokens"),
        };
        days.entry(day).or_default().add_assign(&counts);
    }
    days
}

/// Reduce a set of per-day buckets into cumulative + month-to-date totals.
///
/// `month_prefix` is the current `YYYY-MM` in local time; any day key starting
/// with it counts toward month-to-date. Pure (clock injected by caller).
pub fn summarize(days: &HashMap<String, TokenCounts>, month_prefix: &str) -> TokenUsage {
    let mut out = TokenUsage::default();
    for (day, counts) in days {
        out.cumulative.add_assign(counts);
        if day.starts_with(month_prefix) {
            out.month_to_date.add_assign(counts);
        }
    }
    out
}

/// Recursively collect every `*.jsonl` transcript under `root`.
fn collect_transcript_files(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            collect_transcript_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn load_cache(path: &Path) -> Cache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic write of the cache via `<path>.tmp` + rename.
fn save_cache(path: &Path, cache: &Cache) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_string(cache) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Merge per-day buckets from `src` into `dst` (in place).
fn merge_days(dst: &mut HashMap<String, TokenCounts>, src: &HashMap<String, TokenCounts>) {
    for (day, counts) in src {
        dst.entry(day.clone()).or_default().add_assign(counts);
    }
}

/// Production entry point: scan all Claude Code transcripts (using the
/// incremental cache) and return cumulative + month-to-date token usage.
///
/// Fail-open: a missing projects dir / unreadable cache degrades to zeros
/// rather than failing the whole metrics emission (the textfile collector
/// runs every minute; one bad scan shouldn't blank every other series).
pub fn collect_token_usage() -> TokenUsage {
    collect_token_usage_at(
        &default_projects_dir(),
        &default_cache_path(),
        &Local::now().format("%Y-%m").to_string(),
    )
}

/// Same as `collect_token_usage` but with injected paths + month prefix, so
/// tests can point at a tempdir and pin "now".
pub fn collect_token_usage_at(projects_dir: &Path, cache_path: &Path, month_prefix: &str) -> TokenUsage {
    let mut files = Vec::new();
    collect_transcript_files(projects_dir, &mut files);

    let mut cache = load_cache(cache_path);
    let mut next: HashMap<String, CacheEntry> = HashMap::new();
    let mut all_days: HashMap<String, TokenCounts> = HashMap::new();

    for path in &files {
        let key = path.to_string_lossy().to_string();
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Reuse the cached parse if the file is byte-for-byte unchanged.
        let entry = match cache.files.remove(&key) {
            Some(e) if e.size == size && e.mtime == mtime => e,
            _ => {
                let content = std::fs::read_to_string(path).unwrap_or_default();
                CacheEntry {
                    size,
                    mtime,
                    days: parse_transcript_days(&content),
                }
            }
        };
        merge_days(&mut all_days, &entry.days);
        next.insert(key, entry);
    }

    // Entries left in `cache.files` correspond to transcripts that no longer
    // exist; dropping them keeps the cache from growing without bound.
    let new_cache = Cache { files: next };
    save_cache(cache_path, &new_cache);

    summarize(&all_days, month_prefix)
}

/// Render the token-usage Prometheus textfile lines. Pure + tested; appended
/// to the existing `claude-watch metrics` output by `metrics::cmd_metrics`.
pub fn token_metric_lines(usage: &TokenUsage) -> Vec<String> {
    let c = &usage.cumulative;
    let m = &usage.month_to_date;
    vec![
        "# HELP claude_code_tokens_total Cumulative Claude Code token usage by type across all retained transcripts".to_string(),
        "# TYPE claude_code_tokens_total counter".to_string(),
        format!("claude_code_tokens_total{{type=\"input\"}} {}", c.input),
        format!("claude_code_tokens_total{{type=\"output\"}} {}", c.output),
        format!("claude_code_tokens_total{{type=\"cache_creation\"}} {}", c.cache_creation),
        format!("claude_code_tokens_total{{type=\"cache_read\"}} {}", c.cache_read),
        "".to_string(),
        "# HELP claude_code_tokens_month_to_date Claude Code token usage for the current calendar month (resets on the 1st), by type".to_string(),
        "# TYPE claude_code_tokens_month_to_date gauge".to_string(),
        format!("claude_code_tokens_month_to_date{{type=\"input\"}} {}", m.input),
        format!("claude_code_tokens_month_to_date{{type=\"output\"}} {}", m.output),
        format!("claude_code_tokens_month_to_date{{type=\"cache_creation\"}} {}", m.cache_creation),
        format!("claude_code_tokens_month_to_date{{type=\"cache_read\"}} {}", m.cache_read),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: &str, id: &str, inp: u64, out: u64, cc: u64, cr: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","usage":{{"input_tokens":{inp},"output_tokens":{out},"cache_creation_input_tokens":{cc},"cache_read_input_tokens":{cr}}}}}}}"#
        )
    }

    #[test]
    fn parse_basic_single_turn() {
        let c = line("2026-06-27T05:01:16.669Z", "msg_1", 100, 10, 20, 30);
        let days = parse_transcript_days(&c);
        // Local-day bucket depends on the test runner's TZ; assert on the sole
        // bucket's contents rather than the key.
        assert_eq!(days.len(), 1);
        let counts = days.values().next().unwrap();
        assert_eq!(counts.input, 100);
        assert_eq!(counts.output, 10);
        assert_eq!(counts.cache_creation, 20);
        assert_eq!(counts.cache_read, 30);
    }

    #[test]
    fn dedups_repeated_message_id_within_file() {
        // Same id twice (streaming partial + final) must count ONCE.
        let dup = line("2026-06-27T05:01:16.669Z", "msg_dup", 100, 10, 20, 30);
        let content = format!("{dup}\n{dup}\n");
        let days = parse_transcript_days(&content);
        let counts = days.values().next().unwrap();
        assert_eq!(counts.input, 100, "duplicate id double-counted: {counts:?}");
        assert_eq!(counts.output, 10);
    }

    #[test]
    fn distinct_ids_accumulate() {
        let a = line("2026-06-27T05:01:16.669Z", "msg_a", 100, 10, 0, 0);
        let b = line("2026-06-27T06:01:16.669Z", "msg_b", 50, 5, 0, 0);
        let days = parse_transcript_days(&format!("{a}\n{b}\n"));
        // Same local day → one bucket summing both.
        let counts = days.values().next().unwrap();
        assert_eq!(counts.input, 150);
        assert_eq!(counts.output, 15);
    }

    #[test]
    fn skips_non_assistant_and_marker_lines() {
        let user = r#"{"type":"user","message":{"content":"hi, no usage here"}}"#;
        let summary = r#"{"type":"summary","summary":"x"}"#;
        let asst = line("2026-06-27T05:01:16.669Z", "msg_only", 7, 1, 0, 0);
        let days = parse_transcript_days(&format!("{user}\n{summary}\n{asst}\n"));
        assert_eq!(days.len(), 1);
        assert_eq!(days.values().next().unwrap().input, 7);
    }

    #[test]
    fn counts_turn_without_id() {
        // No message.id → cannot dedup, counted as a singleton.
        let c = r#"{"type":"assistant","timestamp":"2026-06-27T05:01:16.669Z","message":{"usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let days = parse_transcript_days(c);
        assert_eq!(days.values().next().unwrap().input, 5);
    }

    #[test]
    fn skips_turn_without_timestamp() {
        let c = r#"{"type":"assistant","message":{"id":"x","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        assert!(parse_transcript_days(c).is_empty());
    }

    #[test]
    fn summarize_splits_month_to_date() {
        let mut days = HashMap::new();
        days.insert("2026-06-01".to_string(), TokenCounts { input: 10, output: 1, cache_creation: 0, cache_read: 0 });
        days.insert("2026-06-27".to_string(), TokenCounts { input: 20, output: 2, cache_creation: 0, cache_read: 0 });
        days.insert("2026-05-31".to_string(), TokenCounts { input: 5, output: 5, cache_creation: 0, cache_read: 0 });
        let u = summarize(&days, "2026-06");
        assert_eq!(u.cumulative.input, 35);
        assert_eq!(u.cumulative.output, 8);
        assert_eq!(u.month_to_date.input, 30, "only June days count MTD");
        assert_eq!(u.month_to_date.output, 3);
    }

    #[test]
    fn summarize_empty_month_resets() {
        // No transcripts in the current month → month-to-date is all zeros.
        let mut days = HashMap::new();
        days.insert("2026-05-15".to_string(), TokenCounts { input: 99, output: 9, cache_creation: 0, cache_read: 0 });
        let u = summarize(&days, "2026-06");
        assert_eq!(u.cumulative.input, 99);
        assert_eq!(u.month_to_date, TokenCounts::default());
    }

    #[test]
    fn token_metric_lines_render_both_series() {
        let usage = TokenUsage {
            cumulative: TokenCounts { input: 1, output: 2, cache_creation: 3, cache_read: 4 },
            month_to_date: TokenCounts { input: 5, output: 6, cache_creation: 7, cache_read: 8 },
        };
        let joined = token_metric_lines(&usage).join("\n");
        assert!(joined.contains("# TYPE claude_code_tokens_total counter"));
        assert!(joined.contains("claude_code_tokens_total{type=\"input\"} 1"));
        assert!(joined.contains("claude_code_tokens_total{type=\"cache_read\"} 4"));
        assert!(joined.contains("# TYPE claude_code_tokens_month_to_date gauge"));
        assert!(joined.contains("claude_code_tokens_month_to_date{type=\"output\"} 6"));
        assert!(joined.contains("claude_code_tokens_month_to_date{type=\"cache_creation\"} 7"));
    }

    #[test]
    fn collect_uses_and_refreshes_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let slug = projects.join("-home-x");
        let subagents = slug.join("uuid-1").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();

        // One main transcript + one subagent transcript, both June.
        std::fs::write(
            slug.join("uuid-1.jsonl"),
            format!("{}\n", line("2026-06-10T05:00:00.000Z", "m1", 100, 10, 0, 0)),
        )
        .unwrap();
        std::fs::write(
            subagents.join("agent-a.jsonl"),
            format!("{}\n", line("2026-06-11T05:00:00.000Z", "s1", 50, 5, 0, 0)),
        )
        .unwrap();

        let cache = tmp.path().join("cache.json");
        let u1 = collect_token_usage_at(&projects, &cache, "2026-06");
        assert_eq!(u1.cumulative.input, 150);
        assert_eq!(u1.month_to_date.input, 150);
        assert!(cache.exists(), "cache file should be written");

        // Second run with the cache present yields the same numbers (cache hit
        // path is exercised; files are unchanged).
        let u2 = collect_token_usage_at(&projects, &cache, "2026-06");
        assert_eq!(u1, u2);
    }

    #[test]
    fn collect_missing_projects_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let u = collect_token_usage_at(
            &tmp.path().join("does-not-exist"),
            &tmp.path().join("cache.json"),
            "2026-06",
        );
        assert_eq!(u, TokenUsage::default());
    }

    #[test]
    fn collect_drops_stale_cache_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        let f = projects.join("s.jsonl");
        std::fs::write(&f, format!("{}\n", line("2026-06-10T05:00:00.000Z", "m1", 100, 10, 0, 0))).unwrap();
        let cache = tmp.path().join("cache.json");
        let _ = collect_token_usage_at(&projects, &cache, "2026-06");

        // Remove the transcript; the cache entry must be pruned and totals zero.
        std::fs::remove_file(&f).unwrap();
        let u = collect_token_usage_at(&projects, &cache, "2026-06");
        assert_eq!(u, TokenUsage::default());
        let raw = std::fs::read_to_string(&cache).unwrap();
        assert!(!raw.contains("s.jsonl"), "stale entry not pruned: {raw}");
    }
}
