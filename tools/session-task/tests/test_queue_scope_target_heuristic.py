#!/usr/bin/env python3
"""Tests for the add-time scope/target-repo mismatch heuristic in `queue add`.

Context (botchat #2346, 2026-07-24): distinct from the invented-scope-name bug
that `_validate_repo_scope_tokens` already catches. Here the `--scope` names a
REAL repo dir (so dir-validation passes) but a DIFFERENT one from the repo the
task text clearly operates on -- the concrete case being a task editing
`<org>/platform-html-to-pdf` scoped `repo:platform` (the monorepo). Both
are real dirs; the scope is simply the WRONG repo, over-serializing/racing
independent per-repo work.

The heuristic WARNS (never rejects -- Andrew: "prefer warn-loudly over
hard-reject if the heuristic could misfire") to stderr, and the add still
succeeds (rc 0). It must:
  * fire on the clear `<org>/platform-html-to-pdf` scoped `repo:platform`
    case (path mention + explicit repo: scope),
  * NOT false-positive on generic prose, ambiguous multi-repo text, or when the
    scope already covers the named repo (exactly or more-specifically),
  * stay silent when `*` is in scope or the repos dir is absent,
  * stay silent on three shapes that are CORRECT input and used to warn:
    an absolute path under a home directory sharing a configured org's name
    (`/home/<org>/<leaf>/...`), a repo named only to be FORBIDDEN (`do not
    reference <other-repo> paths`), and any task text that mentions a repo the
    scope claims (corroboration) -- while still warning on a genuine mismatch,
    including one in a clause that merely follows a prohibition.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_scope_target_heuristic.py -v

Or directly (no pytest needed):
    python3 tools/session-task/tests/test_queue_scope_target_heuristic.py
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"

# The realistic homelab repo set the heuristic reasons over. Distinctive names
# (with '-') are trusted on a bare mention; short generic names are not.
_REPOS = (
    "claude-watch",
    "platform",
    "platform-html-to-pdf",
    "platform-typesense",
    "botchat",
    "pr-watch",
    "config",  # short/generic -> bare mention NOT trusted
)

_WARN_NEEDLE = "scope/target mismatch"

# The `<org>/<repo>` prefixes the heuristic recognises are CONFIGURATION
# (`CLAUDE_REPO_ORGS`), not baked-in literals -- so the fixtures declare their
# own synthetic org and the suite doubles as a test that the config is what
# drives the matcher. `test_unconfigured_org_prefix_not_detected` pins the
# other half: an org that isn't configured yields no target at all.
_ORG = "acme-sf"


def _env_for_tmp(tmp, *, make_repos=_REPOS, orgs=_ORG):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    if orgs is None:
        env.pop("CLAUDE_REPO_ORGS", None)
    else:
        env["CLAUDE_REPO_ORGS"] = orgs
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    if make_repos is not None:
        for name in make_repos:
            Path(tmp, "repos", name).mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=timeout
    )


def _add(env, desc, scope_args, *extra):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scope_args:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


def test_the_canonical_mismatch_warns():
    """<org>/platform-html-to-pdf task scoped repo:platform -> warn."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            f"fix pagination in {_ORG}/platform-html-to-pdf renderer",
            ["repo:platform"],
        )
        # Warns but still enqueues (rc 0, valid JSON emitted).
        assert r.returncode == 0, (r.returncode, r.stderr)
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "platform-html-to-pdf" in r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:platform"]  # scope unchanged; advisory only


def test_correct_scope_no_warning():
    """Same task correctly scoped repo:platform-html-to-pdf -> no warning."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            f"fix pagination in {_ORG}/platform-html-to-pdf renderer",
            ["repo:platform-html-to-pdf"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_bare_distinctive_repo_name_warns():
    """A bare distinctive repo name (with '-') is a trusted target."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "update the pr-watch dashboard layout", ["repo:botchat"])
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "pr-watch" in r.stderr


def test_generic_prose_does_not_warn():
    """No real repo named -> heuristic stays silent (no false positive)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        for desc in [
            "investigate the flaky test and/or fix the retry logic",
            "write up a design doc for the new alerting tiers",
            "clean up stale logs in the config directory",  # 'config' is short/generic
        ]:
            r = _add(env, desc, ["repo:claude-watch"])
            assert r.returncode == 0, r.stderr
            assert _WARN_NEEDLE not in r.stderr, f"false positive on {desc!r}: {r.stderr}"


def test_short_generic_repo_name_not_trusted_on_bare_mention():
    """A short/generic dir name ('config') is not a bare-mention target."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "tidy up the config values", ["repo:claude-watch"])
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_ambiguous_multi_repo_no_warning():
    """Two unrelated repos named -> ambiguous -> no warning (defer)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "sync shared config between platform-typesense and pr-watch",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_star_scope_never_warns():
    """A universal '*' scope covers everything -> no warning."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, f"touch {_ORG}/platform-html-to-pdf", ["*"])
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_fail_open_when_repos_dir_absent():
    """No repos/ subtree -> can't tell real dirs from prose -> no warning."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp, make_repos=None)
        # scope repo:anything is fail-open on dir-validation too, so pick a
        # free-form scope to isolate the heuristic path.
        r = _add(env, f"edit {_ORG}/platform-html-to-pdf", ["resource:x"])
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_path_scope_covers_target():
    """A path:<target>/... scope counts as covering the named repo."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            f"edit {_ORG}/platform-html-to-pdf/src/render.ts",
            ["path:platform-html-to-pdf/src"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_more_specific_scope_covers_bare_parent_mention():
    """scope repo:platform-typesense covers a bare 'platform' text mention."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Text names only the specific sub-repo; scope claims it. The prefix
        # 'platform' also matches as a dir but collapses to the specific one.
        r = _add(
            env,
            "bump the platform-typesense client version",
            ["repo:platform-typesense"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


# A short/generic repo name is NOT trusted on a bare mention (mode 2), so
# `<org>/config` is detectable ONLY through the org-prefix patterns -- which
# makes it the clean discriminator for whether the org list is config-driven.
_ORG_ONLY_TARGET = "config"


def test_configured_org_prefix_is_detected():
    """`<org>/config` warns when the org IS configured (mode-1 match only)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            f"fix pagination in {_ORG}/{_ORG_ONLY_TARGET} renderer",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert _ORG_ONLY_TARGET in r.stderr


def test_unconfigured_org_prefix_not_detected():
    """The same text stays silent when the org is NOT in CLAUDE_REPO_ORGS.

    Pins that the org list is genuinely config-driven rather than baked in:
    flip the config off and the identical description yields no target.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp, orgs=None)
        r = _add(
            env,
            f"fix pagination in {_ORG}/{_ORG_ONLY_TARGET} renderer",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_configured_org_prefix_list_accepts_multiple():
    """CLAUDE_REPO_ORGS takes a comma-separated list; each entry matches."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp, orgs=f"other-org, {_ORG}")
        r = _add(
            env,
            f"fix pagination in {_ORG}/{_ORG_ONLY_TARGET} renderer",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert _ORG_ONLY_TARGET in r.stderr


def test_home_repos_path_mention_warns():
    """A ~/repos/<repo> path mention is a trusted target."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "edit ~/repos/platform-typesense config", ["repo:platform"])
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "platform-typesense" in r.stderr


# --------------------------------------------------------------------------
# False positives the guards exist to prevent. Each of these is CORRECT input
# -- the scope names the repo the task really operates on -- and the heuristic
# used to warn anyway. A warning that fires on correct input gets tuned out.
# --------------------------------------------------------------------------


def test_home_dir_sharing_an_org_name_is_not_a_forge_reference():
    """`/home/<org>/<leaf>/...` must not be read as `<org>/<repo>`.

    A configured forge org name can also be a home-directory name. Without a
    token-start anchor on the org, every absolute path under that home looks
    like a forge reference, and the leaf segment becomes the "target" whenever
    it collides with a real repo dir name (`config` here). The task is scoped
    correctly; there is nothing to warn about.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            f"copy the template from /home/{_ORG}/{_ORG_ONLY_TARGET}/settings.json "
            f"into the queue tool",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_forge_host_prefixed_org_reference_still_detected():
    """The anchor allows a forge host in front: `<host>/<org>/<repo>` counts.

    Guards the other side of the anchor -- it must exclude a path separator,
    not any prefix at all.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            f"review https://example.com/{_ORG}/platform-html-to-pdf/pull/7",
            ["repo:platform"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "platform-html-to-pdf" in r.stderr


def test_repo_named_only_in_a_prohibition_clause_is_not_a_target():
    """A no-leakage instruction names a repo in order to FORBID it.

    `... ; do not reference <other-repo> paths` used to make the FORBIDDEN repo
    the target and demand a re-scope onto a repo the task never touches. The
    clause split matters here: the text is one line, so splitting on newlines
    alone would not separate the prohibition from the real work.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "fix the spawn gate; do not reference pr-watch paths in the PR body",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_prohibition_in_a_following_sentence_is_not_a_target():
    """Same shape across a sentence boundary rather than a semicolon."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "Refactor the queue tool. Never mention platform-typesense in the "
            "commit message.",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_prohibition_wrapped_across_a_newline_still_covers_its_repo():
    """A cue and the repo it forbids may land on different lines.

    A line wrap is a continuation, so the cue carries to the next segment --
    unlike a semicolon or a sentence end, which do not.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "fix the spawn gate\ndo not reference\npr-watch paths in the PR body",
            ["repo:claude-watch"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_prohibition_does_not_silence_a_target_after_a_semicolon():
    """A cue scopes to its CLAUSE, not to the rest of the text.

    "do not touch <a>; work in <b>" must still detect `<b>`. This is why the
    filter splits clauses instead of poisoning the whole description.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "do not touch botchat; rework the platform-html-to-pdf renderer",
            ["repo:platform"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "platform-html-to-pdf" in r.stderr


def test_scoped_repo_mentioned_in_the_text_vetoes_the_warning():
    """Any mention of a scoped repo is corroboration -> stay silent.

    The scoped repo here (`config`) is too short/generic to be trusted as a
    bare-mention TARGET, so it never becomes a second candidate and the
    ambiguity check cannot save us. It is still perfectly good evidence that
    the scope is deliberate.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "port the retry logic from pr-watch into config",
            [f"repo:{_ORG_ONLY_TARGET}"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE not in r.stderr, r.stderr


def test_unmentioned_scoped_repo_does_not_veto():
    """The veto needs an actual mention, not merely a scope token.

    Pairs with the test above: same target, same shape, but the scoped repo is
    absent from the text -- so the mismatch is still reported.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(
            env,
            "port the retry logic from pr-watch into the queue tool",
            [f"repo:{_ORG_ONLY_TARGET}"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "pr-watch" in r.stderr


def test_prohibition_in_the_description_does_not_reach_the_summary():
    """Description and summary are scanned separately.

    They used to be concatenated with a space, so a description ending in a
    prohibition would have swallowed the start of the summary (and vice
    versa). Here the summary carries the real target and must still be seen.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(
            env,
            "queue",
            "add",
            "tidy the queue tool, and do not touch botchat",
            "--summary",
            "rework the platform-html-to-pdf renderer",
            "--scope",
            "repo:platform",
            "--json",
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "platform-html-to-pdf" in r.stderr


def test_interior_dot_in_a_repo_name_does_not_split_a_clause():
    """`.` is a clause boundary only when whitespace follows it.

    A repo dir may carry interior dots (`claude-watch.bak.v13`); splitting on
    every '.' would cut such a name in half and lose the target.
    """
    with tempfile.TemporaryDirectory() as tmp:
        repos = _REPOS + ("claude-watch.bak.v13",)
        env = _env_for_tmp(tmp, make_repos=repos)
        r = _add(
            env,
            f"restore the queue store from {_ORG}/claude-watch.bak.v13",
            ["repo:platform"],
        )
        assert r.returncode == 0, r.stderr
        assert _WARN_NEEDLE in r.stderr, r.stderr
        assert "claude-watch.bak.v13" in r.stderr


if __name__ == "__main__":
    import traceback

    tests = [v for k, v in list(globals().items()) if k.startswith("test_")]
    failed = 0
    for t in tests:
        try:
            t()
            print(f"PASS {t.__name__}")
        except Exception:
            failed += 1
            print(f"FAIL {t.__name__}")
            traceback.print_exc()
    if failed:
        print(f"\n{failed}/{len(tests)} failed")
        sys.exit(1)
    print(f"\n{len(tests)}/{len(tests)} passed")
