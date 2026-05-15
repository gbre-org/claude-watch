//! One-shot timing harness for the AST predicate spike.
//!
//! Not a `criterion` benchmark (we don't have that dep yet and the spike
//! doesn't need it). Reports median + p99 across N iterations for five
//! representative bash invocations, exercising the parser's warm cache.
//!
//! Run with: `cargo test --release --test bench_ast_predicate -- --nocapture`

use std::time::Instant;

use claude_watch::obligations::ast_predicate::{evaluate, AstPredicate};

const ITERS: usize = 1000;

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

fn percentile(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * pct / 100.0).floor() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn time_one(label: &str, predicate: &AstPredicate, command: &str) {
    // Warmup — also primes the parser singleton.
    for _ in 0..10 {
        let _ = evaluate(predicate, command);
    }

    let mut samples: Vec<u128> = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let _ = evaluate(predicate, command);
        samples.push(t0.elapsed().as_nanos());
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p99 = percentile(&samples, 99.0);
    let max = *samples.last().unwrap();

    println!(
        "{:<40}  median={:>7}ns  p99={:>8}ns  max={:>8}ns  ({} bytes)",
        label,
        median,
        p99,
        max,
        command.len(),
    );
}

#[test]
fn bench_representative_invocations() {
    println!();
    println!("AST predicate benchmark — {} iterations per case, warm cache", ITERS);
    println!("{}", "-".repeat(96));

    // (a) plain signal-send
    time_one(
        "(a) plain signal-send",
        &ban_obligations_bypass(),
        r#"signal-send --dm andrew "hello""#,
    );

    // (b) heredoc-staged signal-send
    let heredoc = "f=$(signal-stage)\n\
                   cat > \"$f\" <<'EOF_X'\n\
                   This is a multi-line message body.\n\
                   It mentions OBLIGATIONS_BYPASS=1 inline as a discussion\n\
                   note, and references piping to tail -20 in passing.\n\
                   EOF_X\n\
                   signal-send --dm andrew -F \"$f\"";
    time_one("(b) heredoc-staged signal-send", &ban_obligations_bypass(), heredoc);

    // (c) long pipeline
    time_one(
        "(c) long pipeline",
        &pipe_to_tail_or_head(),
        "signal-history --group abo | grep ale | sort -u | uniq | wc -l",
    );

    // (d) nested command substitution
    time_one(
        "(d) nested command substitution",
        &pipe_to_tail_or_head(),
        r#"signal-send andrew "today: $(echo "$(date +%F): $(uptime | awk '{print $3}')")""#,
    );

    // (e) multiple OBLIGATIONS_BYPASS mentions inside a heredoc body
    let many = "f=$(signal-stage)\n\
                cat > \"$f\" <<'EOF_X'\n\
                Audit note: ob-2026-05-13 attempted OBLIGATIONS_BYPASS=1.\n\
                Earlier, a user tried OBLIGATIONS_BYPASS=foo then\n\
                OBLIGATIONS_BYPASS=bar; both denied. The OBLIGATIONS_BYPASS\n\
                token here appears five times across the message body for\n\
                emphasis: OBLIGATIONS_BYPASS, OBLIGATIONS_BYPASS.\n\
                EOF_X\n\
                signal-send --dm andrew -F \"$f\"";
    time_one("(e) heredoc w/ many bypass mentions", &ban_obligations_bypass(), many);

    // Also exercise the heredoc-FP DEMO so its timing is recorded.
    let demo = "f=$(signal-stage)\n\
                cat > \"$f\" <<'EOF_X'\n\
                I tried OBLIGATIONS_BYPASS=1 but it was not honored\n\
                EOF_X\n\
                signal-send -F \"$f\"";
    time_one("    heredoc-FP demo (BanEnvVarPrefix)", &ban_obligations_bypass(), demo);

    println!("{}", "-".repeat(96));
}

#[test]
fn heredoc_fp_demo_is_a_pass() {
    // Sanity: the exact heredoc-FP demo command must be ALLOWED by the AST
    // predicate even though the body contains the literal token
    // `OBLIGATIONS_BYPASS=1`. This is the spike's reason for existence.
    let pred = ban_obligations_bypass();
    let cmd = "f=$(signal-stage)\n\
               cat > \"$f\" <<'EOF_X'\n\
               I tried OBLIGATIONS_BYPASS=1 but it was not honored\n\
               EOF_X\n\
               signal-send -F \"$f\"";
    let r = evaluate(&pred, cmd);
    assert!(
        r.satisfied,
        "heredoc body containing OBLIGATIONS_BYPASS=1 must NOT be denied; got {:?}",
        r
    );
}

#[test]
fn surface_string_regex_fp_for_comparison() {
    // For the PR description, show what a regex run against the raw command
    // string would do on the same input — it sees the token in the heredoc
    // body and would flag it as if it were a command prefix.
    //
    // Uses `regex-lite` (already in the workspace deps); pattern matches the
    // same shape as a `no_pipe_pattern`-style regex obligation would.
    let cmd = "f=$(signal-stage)\n\
               cat > \"$f\" <<'EOF_X'\n\
               I tried OBLIGATIONS_BYPASS=1 but it was not honored\n\
               EOF_X\n\
               signal-send -F \"$f\"";
    let re = regex_lite::Regex::new(r"OBLIGATIONS_BYPASS\s*=").unwrap();
    assert!(
        re.is_match(cmd),
        "surface regex SHOULD match this command's text (FP). If this assertion \
         starts failing, the regex was tightened — re-check the spike rationale."
    );

    // Same input, AST predicate — must NOT match (the spike's whole point).
    let pred = ban_obligations_bypass();
    let r = evaluate(&pred, cmd);
    assert!(
        r.satisfied,
        "AST predicate must allow heredoc-body bypass tokens; got {:?}",
        r
    );
}
