//! AST-based obligations predicates — SPIKE.
//!
//! ## Background
//!
//! The obligations system (Python CLI under `tools/obligations/`) currently
//! gates Bash tool calls with regex-based predicates (`no_pipe_pattern`). A
//! representative live rule is `ob-2026-05-13-a411`, which forbids piping
//! `signal-history` output into `tail`/`head` by matching the raw command
//! string against `/\|\s*(tail|head)\s+-/`.
//!
//! That regex approach has a real false-positive class: if a Bash invocation
//! includes a heredoc whose body happens to contain text matching the regex
//! (e.g. drafting a Signal message that mentions the words "tail -20" or
//! "OBLIGATIONS_BYPASS"), the predicate fires even though the offending
//! tokens are message content, not actual command structure.
//!
//! ## Approach
//!
//! Parse the Bash invocation into an AST with `tree-sitter-bash`, then walk
//! the AST and evaluate the predicate against structural nodes (pipelines,
//! commands, variable assignments) rather than the raw byte stream. Heredoc
//! bodies live under their own `heredoc_body` node and are never visited by
//! the command/argument walker.
//!
//! ## Status
//!
//! SPIKE only. Two prototype predicates implemented for comparison against
//! the existing regex versions; existing regex predicates are NOT removed.
//! No integration with the hook hot-path yet — the public surface here is
//! just `evaluate(...)` plus the predicate enum.
//!
//! ## Parser choice
//!
//! Considered:
//!   - `conch-parser` (pure Rust, last released 2017, POSIX-oriented).
//!     Risk: stale, not bash-specific, unclear heredoc support.
//!   - `mvdan/sh` via subprocess (Go binary). Full bash dialect, but adds an
//!     `exec()` per PreToolUse evaluation — multi-millisecond floor plus an
//!     external runtime dependency.
//!   - **`tree-sitter-bash`** (chosen). Actively maintained (0.25.1 at time
//!     of writing), bash-specific, pulled in via the standard `tree-sitter`
//!     Rust binding. Has dedicated grammar nodes for `heredoc_body`,
//!     `heredoc_redirect`, `variable_assignment`, `pipeline`, `command`,
//!     `command_name`, etc. — exactly the structural primitives this spike
//!     needs. In-process, no subprocess overhead.

use std::sync::OnceLock;
use std::sync::Mutex;

use tree_sitter::{Node, Parser, Tree};

/// Predicate kinds covered by this spike.
///
/// The two variants prototype the AST equivalents of:
///   - `ob-2026-05-13-a411` (BAN `signal-history | tail/head -N`)
///   - the inline `OBLIGATIONS_BYPASS=1 cmd` detect (env-var prefix)
///
/// Both are BAN-style: `evaluate` returns `satisfied=true` when the AST does
/// NOT contain the forbidden shape (mirroring how `no_pipe_pattern` semantics
/// work in the Python obligations CLI).
#[derive(Debug, Clone)]
pub enum AstPredicate {
    /// Reject a Bash invocation whose AST contains a pipeline stage whose
    /// `command_name` is exactly one of the listed binaries AND whose
    /// argument list contains a `-N`-style numeric flag (e.g. `-20`, `-n`,
    /// `-n 5`).
    ///
    /// This is the AST equivalent of `no_pipe_pattern` with regex
    /// `/\|\s*(tail|head)\s+-/`. Crucially, it does NOT match
    /// `signal-history --tail 20 ...` (where `tail` is a plain `word`
    /// argument, not a `command_name`).
    BanPipeTo {
        /// Lower-case binary names that must not appear as the command_name
        /// on the right-hand-side of a pipeline.
        bin_names: Vec<String>,
        /// If true, only match when the pipeline stage also carries a
        /// `-`-style flag argument (the current regex requires `\s+-`).
        require_dash_arg: bool,
    },

    /// Reject a Bash invocation that includes a top-level simple command
    /// prefixed with one of the listed env-var assignments
    /// (e.g. `OBLIGATIONS_BYPASS=1 signal-send ...`).
    ///
    /// Crucially, this looks at `variable_assignment` nodes that are direct
    /// children of `command` nodes — NOT raw substring occurrences of the
    /// var name. So an `OBLIGATIONS_BYPASS` token appearing in a heredoc
    /// body, a string literal, or a comment is NOT matched.
    BanEnvVarPrefix {
        /// Env-var names whose assignment as a command prefix is forbidden.
        var_names: Vec<String>,
    },
}

/// Result of evaluating a predicate against a parsed AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalResult {
    /// `true` if the predicate is satisfied (i.e. the command is OK). False
    /// when the predicate matched a forbidden shape.
    pub satisfied: bool,
    /// Human-readable explanation (drop into the obligations deny banner).
    pub why: String,
}

impl EvalResult {
    fn ok(why: impl Into<String>) -> Self {
        Self { satisfied: true, why: why.into() }
    }
    fn deny(why: impl Into<String>) -> Self {
        Self { satisfied: false, why: why.into() }
    }
}

/// Global tree-sitter parser. tree-sitter `Parser` is `!Sync`; gate behind a
/// mutex. Parser creation + language setup is ~microseconds, but we still
/// avoid re-doing it per call.
fn parser() -> &'static Mutex<Parser> {
    static PARSER: OnceLock<Mutex<Parser>> = OnceLock::new();
    PARSER.get_or_init(|| {
        let mut p = Parser::new();
        let lang = tree_sitter_bash::LANGUAGE;
        p.set_language(&lang.into())
            .expect("tree-sitter-bash language load failed");
        Mutex::new(p)
    })
}

/// Parse a Bash command string and evaluate the predicate against it.
///
/// Default-open semantics on parse error (match the Python obligations
/// behavior — a broken predicate must never blackhole the hook).
pub fn evaluate(predicate: &AstPredicate, command: &str) -> EvalResult {
    let tree = match parse(command) {
        Some(t) => t,
        None => {
            return EvalResult::ok(format!(
                "tree-sitter-bash failed to parse (default-open): {} bytes",
                command.len()
            ));
        }
    };

    let root = tree.root_node();
    if root.has_error() {
        // ERROR nodes are common with partial / weird shell — still try the
        // walk; tree-sitter recovers structurally enough for our shapes.
        // But surface it in the `why` so operators can spot pathological
        // inputs.
        let res = walk_root(predicate, &root, command.as_bytes());
        return EvalResult {
            satisfied: res.satisfied,
            why: format!("{} (note: AST contained ERROR nodes)", res.why),
        };
    }
    walk_root(predicate, &root, command.as_bytes())
}

fn parse(command: &str) -> Option<Tree> {
    let guard = parser().lock().ok()?;
    let mut p = guard;
    p.parse(command, None)
}

fn walk_root(predicate: &AstPredicate, root: &Node, src: &[u8]) -> EvalResult {
    match predicate {
        AstPredicate::BanPipeTo { bin_names, require_dash_arg } => {
            check_ban_pipe_to(root, src, bin_names, *require_dash_arg)
        }
        AstPredicate::BanEnvVarPrefix { var_names } => {
            check_ban_env_var_prefix(root, src, var_names)
        }
    }
}

/// Walk every `pipeline` node in the AST. For each pipeline, examine each
/// stage that is NOT the first — those are the consumers on the RHS of `|`.
/// If a consumer is a `command` whose `command_name` is in `bin_names` AND
/// (when `require_dash_arg`) has any argument that starts with `-`, deny.
fn check_ban_pipe_to(
    root: &Node,
    src: &[u8],
    bin_names: &[String],
    require_dash_arg: bool,
) -> EvalResult {
    let mut hits: Vec<String> = Vec::new();
    visit(root, &mut |node| {
        if node.kind() == "pipeline" {
            // tree-sitter-bash represents `a | b | c` as a pipeline whose
            // children are (command, "|", command, "|", command). We treat
            // every command child after the first as a "consumer".
            let mut cursor = node.walk();
            let mut command_idx = 0;
            for child in node.children(&mut cursor) {
                if child.kind() == "command" {
                    command_idx += 1;
                    if command_idx == 1 {
                        continue; // producer side, ignore
                    }
                    if let Some(name) = command_name_of(&child, src) {
                        let name_lc = name.to_ascii_lowercase();
                        if bin_names.iter().any(|b| b.eq_ignore_ascii_case(&name_lc)) {
                            if !require_dash_arg || command_has_dash_arg(&child, src) {
                                hits.push(format!(
                                    "pipeline stage `{}` (with{} dash-arg) matches BanPipeTo",
                                    name_lc,
                                    if require_dash_arg { "" } else { "out checking" },
                                ));
                            }
                        }
                    }
                }
            }
        }
    });

    if hits.is_empty() {
        EvalResult::ok(format!(
            "no forbidden pipe-to {:?} found (require_dash_arg={})",
            bin_names, require_dash_arg
        ))
    } else {
        EvalResult::deny(format!("BanPipeTo: {}", hits.join("; ")))
    }
}

/// Walk every `command` node in the AST. For each, examine its child
/// `variable_assignment` nodes (these are the `FOO=bar` prefixes on a simple
/// command). If any assignment's `variable_name` is in `var_names`, deny.
///
/// `variable_assignment` nodes ALSO appear as standalone statements
/// (`FOO=bar` on its own line) — those are NOT what we're after. We only
/// flag assignments that are children of a `command` node (i.e. command
/// prefixes).
fn check_ban_env_var_prefix(
    root: &Node,
    src: &[u8],
    var_names: &[String],
) -> EvalResult {
    let mut hits: Vec<String> = Vec::new();
    visit(root, &mut |node| {
        if node.kind() == "command" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_assignment" {
                    if let Some(vname) = first_named_child_text(&child, "variable_name", src) {
                        if var_names.iter().any(|v| v == &vname) {
                            hits.push(format!(
                                "command-prefix env-var assignment `{}=...`",
                                vname
                            ));
                        }
                    }
                }
            }
        }
    });

    if hits.is_empty() {
        EvalResult::ok(format!(
            "no forbidden command-prefix env-var assignments {:?} found",
            var_names
        ))
    } else {
        EvalResult::deny(format!("BanEnvVarPrefix: {}", hits.join("; ")))
    }
}

/// Visit every node in the tree (DFS pre-order), invoking `f` on each.
fn visit<'a, F>(node: &Node<'a>, f: &mut F)
where
    F: FnMut(&Node<'a>),
{
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(&child, f);
    }
}

/// Extract the `command_name` text of a `command` node. Returns None if the
/// command has no `command_name` child (unusual but possible for
/// assignment-only "commands" like `FOO=bar`).
fn command_name_of(command: &Node, src: &[u8]) -> Option<String> {
    let mut cursor = command.walk();
    for child in command.children(&mut cursor) {
        if child.kind() == "command_name" {
            // command_name wraps a single word/string/expansion. Take the
            // node's full text.
            return node_text(&child, src);
        }
    }
    None
}

/// True if the command node has any non-name argument starting with `-`.
fn command_has_dash_arg(command: &Node, src: &[u8]) -> bool {
    let mut cursor = command.walk();
    let mut seen_name = false;
    for child in command.children(&mut cursor) {
        if child.kind() == "command_name" {
            seen_name = true;
            continue;
        }
        if !seen_name {
            // skip pre-name variable_assignments
            continue;
        }
        // Argument forms: `word`, `string`, `raw_string`, `number`,
        // `concatenation`, etc. We check the leading byte of the text.
        if let Some(text) = node_text(&child, src) {
            if text.starts_with('-') {
                return true;
            }
        }
    }
    false
}

/// Find the first named-child node whose `kind` matches `target`, and return
/// its text.
fn first_named_child_text(node: &Node, target: &str, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == target {
            return node_text(&child, src);
        }
    }
    None
}

fn node_text(node: &Node, src: &[u8]) -> Option<String> {
    let slice = src.get(node.start_byte()..node.end_byte())?;
    std::str::from_utf8(slice).ok().map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe_to_tail_or_head() -> AstPredicate {
        AstPredicate::BanPipeTo {
            bin_names: vec!["tail".into(), "head".into()],
            require_dash_arg: true,
        }
    }

    fn ban_obligations_bypass() -> AstPredicate {
        AstPredicate::BanEnvVarPrefix {
            var_names: vec!["OBLIGATIONS_BYPASS".into()],
        }
    }

    // -- BanPipeTo true positives -------------------------------------------

    #[test]
    fn ban_pipe_to_tail_dash_n() {
        let r = evaluate(&pipe_to_tail_or_head(), "signal-history --group abo | tail -20");
        assert!(!r.satisfied, "should deny pipe to tail: {:?}", r);
    }

    #[test]
    fn ban_pipe_to_head_dash_n() {
        let r = evaluate(&pipe_to_tail_or_head(), "signal-history --group abo | head -n 5");
        assert!(!r.satisfied, "should deny pipe to head -n: {:?}", r);
    }

    #[test]
    fn ban_pipe_to_tail_through_grep() {
        let r = evaluate(
            &pipe_to_tail_or_head(),
            "signal-history --group abo | grep ale | tail -5",
        );
        assert!(!r.satisfied, "should deny multi-stage pipe ending in tail: {:?}", r);
    }

    // -- BanPipeTo true negatives -------------------------------------------

    #[test]
    fn allow_signal_history_with_tail_flag() {
        // `--tail` is a flag, not a pipe target. The current regex
        // /\|\s*(tail|head)\s+-/ ALSO allows this, but the AST version must
        // continue to allow it: `tail` here is a plain `word` arg, not a
        // command_name.
        let r = evaluate(&pipe_to_tail_or_head(), "signal-history --tail 20 --group abo");
        assert!(r.satisfied, "should allow --tail arg: {:?}", r);
    }

    #[test]
    fn allow_bare_signal_history() {
        let r = evaluate(&pipe_to_tail_or_head(), "signal-history --group abo");
        assert!(r.satisfied, "should allow bare signal-history: {:?}", r);
    }

    #[test]
    fn allow_pipe_to_other_consumer() {
        let r = evaluate(&pipe_to_tail_or_head(), "signal-history --group abo | grep ale");
        assert!(r.satisfied, "should allow pipe to grep: {:?}", r);
    }

    #[test]
    fn allow_tail_in_heredoc_body() {
        // CORE SPIKE DEMO: heredoc body contains literal text "| tail -20"
        // but is structurally a heredoc_body, not a pipeline.
        let cmd = "f=$(signal-stage)\n\
                   cat > \"$f\" <<'EOF_X'\n\
                   debug log fragment: \"signal-history --group abo | tail -20\"\n\
                   EOF_X\n\
                   signal-send -F \"$f\"";
        let r = evaluate(&pipe_to_tail_or_head(), cmd);
        assert!(
            r.satisfied,
            "heredoc body containing pipe-to-tail text must NOT match: {:?}",
            r
        );
    }

    #[test]
    fn allow_tail_in_single_quoted_string() {
        // Edge: literal string arg containing the forbidden text.
        let r = evaluate(
            &pipe_to_tail_or_head(),
            "signal-send andrew 'snippet: cmd | tail -20'",
        );
        assert!(r.satisfied, "tail inside string literal must NOT match: {:?}", r);
    }

    #[test]
    fn allow_tail_in_command_substitution_arg() {
        // `$(printf '... | tail -20 ...')` — the substitution's stdout is
        // a string in the outer command's arg list. The inner `printf` is
        // a command_name `printf`, not tail.
        let r = evaluate(
            &pipe_to_tail_or_head(),
            "signal-send andrew \"$(printf 'how about cmd | tail -20')\"",
        );
        assert!(
            r.satisfied,
            "tail text inside command substitution arg must NOT match: {:?}",
            r
        );
    }

    // -- BanEnvVarPrefix true positives -------------------------------------

    #[test]
    fn ban_obligations_bypass_prefix() {
        let r = evaluate(&ban_obligations_bypass(), "OBLIGATIONS_BYPASS=1 signal-send foo");
        assert!(!r.satisfied, "should deny OBLIGATIONS_BYPASS prefix: {:?}", r);
    }

    #[test]
    fn ban_obligations_bypass_prefix_multi() {
        let r = evaluate(
            &ban_obligations_bypass(),
            "OBLIGATIONS_BYPASS=1 FOO=bar signal-send hi",
        );
        assert!(
            !r.satisfied,
            "should deny OBLIGATIONS_BYPASS prefix among multi-prefix: {:?}",
            r
        );
    }

    // -- BanEnvVarPrefix true negatives (the FP fix the spike exists for) ---

    #[test]
    fn allow_obligations_bypass_in_heredoc_body() {
        // CORE SPIKE DEMO: the entire reason this spike exists.
        let cmd = "f=$(signal-stage)\n\
                   cat > \"$f\" <<'EOF_X'\n\
                   I tried OBLIGATIONS_BYPASS=1 but it was not honored\n\
                   EOF_X\n\
                   signal-send -F \"$f\"";
        let r = evaluate(&ban_obligations_bypass(), cmd);
        assert!(
            r.satisfied,
            "OBLIGATIONS_BYPASS in heredoc body must NOT match: {:?}",
            r
        );
    }

    #[test]
    fn allow_obligations_bypass_in_single_quoted_string() {
        let r = evaluate(
            &ban_obligations_bypass(),
            "signal-send andrew 'note about OBLIGATIONS_BYPASS=1 history'",
        );
        assert!(
            r.satisfied,
            "OBLIGATIONS_BYPASS in single-quoted string arg must NOT match: {:?}",
            r
        );
    }

    #[test]
    fn allow_obligations_bypass_in_double_quoted_string() {
        let r = evaluate(
            &ban_obligations_bypass(),
            "signal-send andrew \"note about OBLIGATIONS_BYPASS=1 attempt\"",
        );
        assert!(
            r.satisfied,
            "OBLIGATIONS_BYPASS in double-quoted string arg must NOT match: {:?}",
            r
        );
    }

    #[test]
    fn allow_obligations_bypass_in_comment() {
        let r = evaluate(
            &ban_obligations_bypass(),
            "signal-send andrew hi # OBLIGATIONS_BYPASS=1 would be cheating",
        );
        assert!(
            r.satisfied,
            "OBLIGATIONS_BYPASS in trailing comment must NOT match: {:?}",
            r
        );
    }

    // -- BanEnvVarPrefix edge: pipeline-prefix env-var ----------------------

    #[test]
    fn ban_env_var_prefix_on_pipeline_first_stage() {
        // `FOO=1 cmd | other` — the env-var is on the FIRST stage of the
        // pipeline. tree-sitter-bash represents this as
        // pipeline > command > variable_assignment, which our walker
        // descends into via DFS, so this should still be denied.
        let r = evaluate(
            &ban_obligations_bypass(),
            "OBLIGATIONS_BYPASS=1 signal-send foo | tee log",
        );
        assert!(
            !r.satisfied,
            "OBLIGATIONS_BYPASS as pipeline-first-stage prefix must match: {:?}",
            r
        );
    }

    // -- pathological / robustness ------------------------------------------

    #[test]
    fn empty_command_is_allowed() {
        let r = evaluate(&pipe_to_tail_or_head(), "");
        assert!(r.satisfied);
    }

    #[test]
    fn comment_only_is_allowed() {
        let r = evaluate(&pipe_to_tail_or_head(), "# just a comment | tail -20");
        assert!(r.satisfied);
    }

    #[test]
    fn nested_command_substitution_pipe_to_tail_still_caught() {
        // Inner pipeline -> tail. The DFS visitor finds the inner pipeline
        // node regardless of nesting depth.
        let r = evaluate(
            &pipe_to_tail_or_head(),
            "echo \"$(signal-history --group abo | tail -20)\"",
        );
        assert!(
            !r.satisfied,
            "inner pipe-to-tail inside command substitution should still match: {:?}",
            r
        );
    }
}
