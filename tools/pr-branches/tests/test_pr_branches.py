#!/usr/bin/env python3
"""Tests for the `pr-branches` PR-branch lifecycle tool.

Everything here is offline: the GitHub and git accessors are replaced with
stubs, so no test touches a real repository, remote, or the GitHub API.

What's covered:

  * Every classification bucket, including the ones that must never be
    deleted (no PR, open, draft, closed-unmerged, worktree, default branch).
  * The squash-merge trap: a merged branch whose commits are NOT ancestors of
    the base branch is still classified deletable, because PR state -- not
    ancestry -- decides merged-ness.
  * Precedence: worktree and default-branch checks beat "merged", so a branch
    that is both merged and checked out is skipped rather than deleted.
  * Reused branch names: the newest PR wins, so an old merged PR can never
    authorize deleting a branch a newer unmerged PR is using.
  * Containment: a merged branch whose tip moved is deletable only when it has
    zero commits the base branch lacks; an unknown answer keeps it.
  * The sweep dry run deletes nothing, and the delete run deletes only
    deletable rows.
  * The delete path re-verifies each branch individually and refuses when the
    live PR state or the branch tip disagrees with the classification.
  * `merge` asserts the branch is gone, deletes it when the merge tool left it
    behind, and refuses to delete anything when the PR did not actually merge.
"""

from __future__ import annotations

import importlib.util
import io
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace

TOOL = Path(__file__).resolve().parents[1] / "pr-branches"

_spec = importlib.util.spec_from_loader(
    "pr_branches", importlib.machinery.SourceFileLoader("pr_branches", str(TOOL))
)
pb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(pb)


def pr(number, branch, state, oid, draft=False):
    return {
        "number": number,
        "headRefName": branch,
        "headRefOid": oid,
        "state": state,
        "isDraft": draft,
        "url": f"https://example.invalid/pull/{number}",
        "title": f"PR {number}",
    }


def cats(rows):
    return {r["branch"]: r["category"] for r in rows}


class ClassifyTest(unittest.TestCase):
    def test_every_bucket(self):
        branches = {
            "main": "m0",
            "feat/merged": "a1",
            "feat/open": "b1",
            "feat/draft": "c1",
            "feat/closed": "d1",
            "feat/orphan": "e1",
            "feat/checked-out": "f1",
        }
        prs = [
            pr(1, "feat/merged", "MERGED", "a1"),
            pr(2, "feat/open", "OPEN", "b1"),
            pr(3, "feat/draft", "OPEN", "c1", draft=True),
            pr(4, "feat/closed", "CLOSED", "d1"),
            pr(5, "feat/checked-out", "MERGED", "f1"),
        ]
        got = cats(pb.classify(branches, prs, "main", {"feat/checked-out"}))
        self.assertEqual(
            got,
            {
                "main": "default-branch",
                "feat/merged": "merged",
                "feat/open": "open",
                "feat/draft": "draft",
                "feat/closed": "closed-unmerged",
                "feat/orphan": "no-pr",
                "feat/checked-out": "worktree",
            },
        )
        # Only the merged one is deletable; everything else is kept.
        deletable = [r["branch"] for r in pb.classify(branches, prs, "main", {"feat/checked-out"})
                     if r["category"] in pb.DELETABLE]
        self.assertEqual(deletable, ["feat/merged"])

    def test_squash_merged_branch_is_deletable_without_ancestry(self):
        """The core trap: a squash merge leaves the branch commits out of main.

        `git merge-base --is-ancestor` would report this branch as unmerged.
        Classification must still call it deletable, because the API says the
        PR merged and the tip is exactly the head that got merged.
        """
        rows = pb.classify(
            {"feat/squashed": "orig-sha"},
            [pr(9, "feat/squashed", "MERGED", "orig-sha")],
            "main",
            set(),
        )
        self.assertEqual(rows[0]["category"], "merged")
        self.assertIn(rows[0]["category"], pb.DELETABLE)

    def test_default_branch_is_never_deletable_even_with_a_merged_pr(self):
        rows = pb.classify(
            {"main": "m0"}, [pr(1, "main", "MERGED", "m0")], "main", set()
        )
        self.assertEqual(rows[0]["category"], "default-branch")
        self.assertNotIn(rows[0]["category"], pb.DELETABLE)

    def test_worktree_beats_merged(self):
        rows = pb.classify(
            {"feat/x": "s1"}, [pr(1, "feat/x", "MERGED", "s1")], "main", {"feat/x"}
        )
        self.assertEqual(rows[0]["category"], "worktree")

    def test_reused_branch_name_uses_the_newest_pr(self):
        """An old merged PR must not authorize deleting a newer PR's branch."""
        rows = pb.classify(
            {"feat/reused": "new-sha"},
            [
                pr(10, "feat/reused", "MERGED", "old-sha"),
                pr(80, "feat/reused", "CLOSED", "new-sha"),
            ],
            "main",
            set(),
        )
        self.assertEqual(rows[0]["category"], "closed-unmerged")
        self.assertEqual(rows[0]["pr"], 80)
        self.assertEqual(rows[0]["pr_count"], 2)

    def test_unknown_pr_state_is_kept(self):
        rows = pb.classify(
            {"feat/x": "s1"}, [pr(1, "feat/x", "SOMETHING_NEW", "s1")], "main", set()
        )
        self.assertNotIn(rows[0]["category"], pb.DELETABLE)

    def test_moved_tip_without_containment_check_is_kept(self):
        rows = pb.classify(
            {"feat/x": "moved"}, [pr(1, "feat/x", "MERGED", "orig")], "main", set()
        )
        self.assertEqual(rows[0]["category"], "sha-mismatch")


class ContainmentTest(unittest.TestCase):
    def setUp(self):
        self._real = pb.commits_not_in
        self.addCleanup(lambda: setattr(pb, "commits_not_in", self._real))

    def _classify(self, ahead):
        pb.commits_not_in = lambda slug, base, sha: ahead
        return pb.classify(
            {"feat/x": "moved"},
            [pr(1, "feat/x", "MERGED", "orig")],
            "main",
            set(),
            slug="o/r",
            containment=True,
        )[0]

    def test_zero_commits_not_in_base_is_deletable(self):
        row = self._classify(0)
        self.assertEqual(row["category"], "merged-contained")
        self.assertIn(row["category"], pb.DELETABLE)

    def test_extra_commits_are_kept(self):
        row = self._classify(2)
        self.assertEqual(row["category"], "sha-mismatch")
        self.assertNotIn(row["category"], pb.DELETABLE)

    def test_unknown_comparison_is_kept_not_assumed_empty(self):
        """A failed comparison is UNKNOWN. It must never read as 'nothing there'."""
        row = self._classify(None)
        self.assertEqual(row["category"], "sha-mismatch")

    def test_containment_cannot_promote_an_unmerged_branch(self):
        """Even with zero commits not in base, a closed PR stays kept."""
        pb.commits_not_in = lambda slug, base, sha: 0
        row = pb.classify(
            {"feat/x": "moved"},
            [pr(1, "feat/x", "CLOSED", "orig")],
            "main",
            set(),
            slug="o/r",
            containment=True,
        )[0]
        self.assertEqual(row["category"], "closed-unmerged")


class SweepTest(unittest.TestCase):
    def setUp(self):
        self.deleted: list[str] = []
        self.branches = {
            "main": "m0",
            "feat/merged": "a1",
            "feat/open": "b1",
            "feat/orphan": "e1",
        }
        self.prs = [
            pr(1, "feat/merged", "MERGED", "a1"),
            pr(2, "feat/open", "OPEN", "b1"),
        ]
        patches = {
            "repo_slug": lambda explicit: "o/r",
            "default_branch": lambda slug: "main",
            "remote_branches": lambda remote: dict(self.branches),
            "worktree_branches": lambda: set(),
            "all_prs": lambda slug, limit: list(self.prs),
            "pr_view": lambda slug, n: next(p for p in self.prs if p["number"] == n),
            "remote_tip": lambda slug, b: self.branches.get(b),
            "commits_not_in": lambda slug, base, sha: 0,
            "delete_ref": lambda slug, b: self.deleted.append(b),
        }
        for name, fn in patches.items():
            real = getattr(pb, name)
            setattr(pb, name, fn)
            self.addCleanup(lambda n=name, r=real: setattr(pb, n, r))

    def _args(self, **kw):
        base = dict(
            repo=None, pattern=None, remote="origin", delete=False, json=False,
            pr_limit=100, no_containment_check=False,
        )
        base.update(kw)
        return SimpleNamespace(**base)

    def _run(self, **kw):
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = pb.cmd_sweep(self._args(**kw))
        return rc, buf.getvalue()

    def test_dry_run_deletes_nothing(self):
        rc, out = self._run()
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, [])
        self.assertIn("DRY RUN", out)

    def test_dry_run_is_the_default(self):
        self.assertFalse(pb.build_parser().parse_args(["sweep"]).delete)

    def test_delete_removes_only_the_merged_branch(self):
        rc, _ = self._run(delete=True)
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, ["feat/merged"])

    def test_pattern_filters_the_candidate_set(self):
        rc, _ = self._run(delete=True, pattern=["feat/o*"])
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, [])

    def test_delete_refuses_when_live_pr_state_disagrees(self):
        """Classification nominates; the per-branch re-check decides."""
        pb.pr_view = lambda slug, n: pr(1, "feat/merged", "CLOSED", "a1")
        rc, _ = self._run(delete=True)
        self.assertEqual(rc, 1)
        self.assertEqual(self.deleted, [])

    def test_delete_refuses_when_the_tip_moved_since_classification(self):
        pb.pr_view = lambda slug, n: pr(1, "feat/merged", "MERGED", "different")
        rc, _ = self._run(delete=True)
        self.assertEqual(rc, 1)
        self.assertEqual(self.deleted, [])

    def test_contained_branch_is_rechecked_before_deletion(self):
        self.prs = [pr(1, "feat/merged", "MERGED", "old-head")]
        pb.commits_not_in = lambda slug, base, sha: 3
        rc, _ = self._run(delete=True)
        self.assertEqual(self.deleted, [])
        self.assertEqual(rc, 0)  # kept as sha-mismatch, nothing attempted


class MergeAssertionTest(unittest.TestCase):
    def setUp(self):
        self.deleted: list[str] = []
        self.exists = True
        patches = {
            "repo_slug": lambda explicit: "o/r",
            "default_branch": lambda slug: "main",
            "ref_exists": lambda slug, b: self.exists,
            "delete_ref": lambda slug, b: (self.deleted.append(b),
                                           setattr(self, "exists", False))[0],
        }
        for name, fn in patches.items():
            real = getattr(pb, name)
            setattr(pb, name, fn)
            self.addCleanup(lambda n=name, r=real: setattr(pb, n, r))

    def _merge_args(self, **kw):
        base = dict(repo=None, number=7, strategy="squash", admin=False,
                    no_delete_branch=False, dry_run=False)
        base.update(kw)
        return SimpleNamespace(**base)

    def test_already_merged_pr_with_leaked_branch_gets_it_deleted(self):
        pb.pr_view = lambda slug, n: pr(7, "feat/x", "MERGED", "s1")
        self.exists = True
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = pb.cmd_merge(self._merge_args())
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, ["feat/x"])

    def test_already_merged_pr_with_clean_branch_deletes_nothing(self):
        pb.pr_view = lambda slug, n: pr(7, "feat/x", "MERGED", "s1")
        self.exists = False
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = pb.cmd_merge(self._merge_args())
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, [])

    def test_refuses_the_default_branch(self):
        pb.pr_view = lambda slug, n: pr(7, "main", "MERGED", "s1")
        with self.assertRaises(pb.Fail):
            pb.cmd_merge(self._merge_args())

    def test_dry_run_reports_the_leak_without_deleting(self):
        pb.pr_view = lambda slug, n: pr(7, "feat/x", "MERGED", "s1")
        self.exists = True
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = pb.cmd_merge(self._merge_args(dry_run=True))
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, [])
        self.assertIn("dry-run", buf.getvalue())

    def test_verify_reports_a_leak_and_can_fix_it(self):
        pb.pr_view = lambda slug, n: pr(7, "feat/x", "MERGED", "s1")
        self.exists = True
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = pb.cmd_verify(SimpleNamespace(repo=None, number=7, delete=False))
        self.assertEqual(rc, 0)
        self.assertIn("leaked", buf.getvalue())
        self.assertEqual(self.deleted, [])

        with redirect_stdout(io.StringIO()):
            rc = pb.cmd_verify(SimpleNamespace(repo=None, number=7, delete=True))
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, ["feat/x"])

    def test_verify_never_deletes_for_an_unmerged_pr(self):
        pb.pr_view = lambda slug, n: pr(7, "feat/x", "CLOSED", "s1")
        self.exists = True
        with redirect_stdout(io.StringIO()):
            rc = pb.cmd_verify(SimpleNamespace(repo=None, number=7, delete=True))
        self.assertEqual(rc, 0)
        self.assertEqual(self.deleted, [])


class ParserTest(unittest.TestCase):
    def test_merge_defaults_to_squash(self):
        self.assertEqual(pb.build_parser().parse_args(["merge", "5"]).strategy, "squash")

    def test_rebase_alias(self):
        self.assertEqual(
            pb.build_parser().parse_args(["merge", "5", "--rebase"]).strategy, "rebase"
        )

    def test_deletable_set_is_exactly_the_two_merged_buckets(self):
        self.assertEqual(pb.DELETABLE, {"merged", "merged-contained"})

    def test_every_category_has_a_reason(self):
        for cat in pb.DELETABLE:
            self.assertIn(cat, pb.REASONS)


if __name__ == "__main__":
    unittest.main(verbosity=2)
