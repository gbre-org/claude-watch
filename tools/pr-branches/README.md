# pr-branches

PR branch lifecycle tooling: keep the remote free of branches whose pull
request has already been merged, and make the merge path prove it did its job.

```
pr-branches merge <PR#>    # merge, then ASSERT the remote branch is gone
pr-branches verify <PR#>   # did a merged PR's branch actually get deleted?
pr-branches sweep          # classify every remote branch; delete merged-PR ones
```

## The bug this exists for

`gh pr merge --squash --delete-branch` does two things in one step: it deletes
the remote branch and it cleans up the local branch. The local half can fail —
most often because the branch is still checked out in a git worktree. When that
happens `gh` exits non-zero and **the abort happens before the remote ref is
deleted**, so the remote branch survives while the failure reads like a
cosmetic local-cleanup nit that is easy to wave off.

Nothing downstream notices, so the remote quietly accumulates dead branches.

`merge` closes the hole at the source. It re-reads the PR after the merge
attempt and, if the PR really did merge but the branch ref is still there,
deletes the ref and re-verifies it is gone. The exit code of the underlying
merge is deliberately not trusted in either direction: non-zero can mean "the
merge landed but cleanup failed", and zero is not by itself proof the ref went
away. Only a positive check of the ref settles it.

`sweep` cleans up whatever backlog accrued before the assertion existed. It is
a backstop, not the plan — if `merge` is used consistently, `sweep` should find
nothing.

## Why not just test whether the branch is merged into main?

Because that test is wrong here, and wrong in the direction that loses work.

A squash merge replays the whole branch as a **single new commit** on the base
branch. The branch's own commits never become ancestors of the base. So
`git merge-base --is-ancestor <branch> main` reports **not merged** for every
squash-merged branch in the repo. A cleanup tool built on that check either
keeps everything (useless) or, with the sense inverted, deletes live work
(catastrophic).

**Pull request state is the authority.** A branch is deleted only when the API
reports a pull request for it whose state is `MERGED`.

Ancestry does get used, for one narrower and different job: proving that
deleting a branch cannot lose anything. It is consulted only for branches whose
PR is already known merged, so it can shrink or refine the delete set but never
promote a branch that PR state did not clear. An unknown answer keeps.

## Classification

`sweep` puts every remote branch in exactly one bucket. Two are deletable:

| category | deleted | meaning |
| --- | --- | --- |
| `merged` | yes | PR merged and the branch tip is still exactly the merged head |
| `merged-contained` | yes | PR merged; the tip moved but is an ancestor of the base branch, so it holds no commit the base lacks |
| `sha-mismatch` | no | PR merged but the tip has commits the base branch does not contain — possibly unmerged work |
| `closed-unmerged` | no | PR was closed without merging |
| `open` / `draft` | no | PR is still open |
| `no-pr` | no | no PR was ever opened for this branch |
| `worktree` | no | branch is checked out in a local git worktree |
| `default-branch` | no | the repository default branch |

`merged-contained` exists because of the same squash-merge behaviour. After a
squash merge the branch often keeps moving — a later `Merge branch 'main' into
<branch>` fast-forwards it past the head that was recorded on the PR. The tip
then no longer matches the merged head even though the branch holds nothing of
its own. Asking the API how many commits are reachable from the tip but not
from the base branch separates the two cases: zero means there is provably
nothing to lose; anything else keeps the branch.

Pass `--no-containment-check` to skip that question entirely, which costs fewer
API calls and keeps strictly more branches.

## Safety rules

* Delete only on a positive `MERGED` from the API. No PR, open, draft, and
  closed-without-merging all mean keep — an unmerged branch may be the only
  copy of someone's work.
* **Dry run by default.** `sweep` prints the full classification and changes
  nothing unless `--delete` is passed.
* The default branch is never touched and nothing here force-pushes.
* Branches checked out in a local worktree are skipped, since that is the exact
  condition that made the merge tool bail in the first place.
* When a branch name has been reused across several PRs, the newest PR wins. An
  old merged PR must never authorize deleting the branch a newer unmerged PR is
  using.
* Every delete candidate is re-verified against the API **individually**,
  immediately before its ref is removed: PR still `MERGED`, head branch still
  the expected one, tip unchanged (or still contained). The bulk PR listing
  only nominates candidates; the per-branch check is what authorizes the
  delete. A disagreement is reported as a failure and the branch is left alone.
* An API call that fails is UNKNOWN, never "nothing there". Unknown keeps.

## Usage

Classify without touching anything:

```
pr-branches sweep
pr-branches sweep --pattern 'feature/*' --pattern 'fix/*'
pr-branches sweep --json          # machine-readable, no epilogue
```

Delete the merged-PR branches:

```
pr-branches sweep --delete
```

Merge a PR the safe way:

```
pr-branches merge 123             # squash by default, then assert
pr-branches merge 123 --rebase
pr-branches merge 123 --dry-run   # show what would happen
```

Audit a single PR after the fact:

```
pr-branches verify 123
pr-branches verify 123 --delete   # clean up a leaked branch
```

Exit codes: `0` success (dry runs included), `1` the operation failed (the
merge did not land, a ref could not be deleted, a re-verification disagreed),
`2` usage or environment error.

## Tests

```
python3 tools/pr-branches/tests/test_pr_branches.py
# or:
make test-pr-branches
```

Fully offline — the GitHub and git accessors are stubbed, so no test touches a
real repository, remote, or the API. Coverage includes every classification
bucket, the squash-merge trap, worktree and default-branch precedence, reused
branch names, containment (including the unknown-answer case), the dry-run
default, and the delete path refusing when live state disagrees.
