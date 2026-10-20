# Design: derive a queue item's scope from its worktree/branch

Status: DESIGN ONLY (no implementation). Scope of this doc: feasibility, data
model, rules, compatibility, tests, migration, size, open decisions.

## 1. Problem

Queue items serialize on `scope` tokens. Work on a repo is almost always
scoped `repo:<name>`, so two items on DIFFERENT branches of one repo (each in
its own git worktree) serialize needlessly, while the scope is hand-typed and
can drift or be invented. Wanted:

* An item that clearly belongs to a worktree/branch gets its scope DERIVED
  from it, and that derived token cannot be overridden or replaced.
* Manual ADDITIONAL scopes (`hostjob:`, `resource:`, `tool:`, `cluster:` ...)
  can always be appended.
* Same branch -> same derived scope (serialize); different branches ->
  different scopes (parallel).

## 2. How scopes and worktrees are modelled today

All paths relative to the repo root.

| Concern | Location |
|---|---|
| Queue schema version (`QUEUE_SCHEMA_VERSION = 2`), stamped on every write | `tools/session-task/session-task` ~L198, `_queue_write_and_unlock` ~L1245 |
| Token normalization (`file:`, `path:`, typed `repo|resource|book|agent-proto`, `task:`; unknown tokens accepted verbatim) | `_normalize_token` ~L1309, `_TYPED_TOKEN_RE` ~L1274 |
| `repo:` validation against the repos dir (fail-open, env bypass) | `_validate_repo_scope_tokens` ~L1386; `SESSION_TASK_REPOS_DIR` ~L535 |
| Add-time scope/target mismatch WARNING (prose names repo X, scope says Y) | `_scope_target_mismatch_warning` ~L1701, called from `cmd_queue_add` ~L3864 |
| Overlap rules (`*`, `file:` prefix, `path:` segment prefix, `repo:X` covers `path:X/..`, everything else exact) | `_tokens_overlap` ~L1790 |
| Serialization set (drops inert `task:` tokens), group merge, running-conflict | `_serializing_scope` ~L1861, `_group_ids_overlapping` ~L1916, `_running_scope_conflicts` ~L1942, `_item_is_ready` ~L2041 |
| `queue add` (item dict built ~L3980; fields: id, description, summary, scope, group_id, status, priority, created_*, optional context) | `cmd_queue_add` ~L3699 |
| Scope delta ops (`update-scope`: add default, `--remove`; no protected tokens) | `cmd_queue_update_scope` ~L7301 |
| Spawn-time gate: reads item `status` + `scope`, does its own scope/target-repo check on the agent prompt | `tools/hooks/pre-agent-queue-gate-hook` (`_scope_repo_names` ~L555, `_scope_target_mismatch` ~L593; `.worktrees` already listed as a non-repo path segment ~L165) |
| q-id <-> agent-id binding at spawn (regex of `Queue item: q-XXXX` in prompt) | `tools/hooks/post-tool-agent-arm-hook` |
| Docs of tokens | `docs/queue.md` "Scope tokens" |
| Test style: subprocess-driven pytest with a temp `HOME` and `conftest.py` | `tools/session-task/tests/test_queue_update_scope.py`, `test_queue_scope_path.py`, `test_queue_repo_scope_validation.py`, `test_queue_scope_target_heuristic.py` |

Worktrees: NOTHING in claude-watch models them. There is no item field linking
an item to a worktree or branch, and no worktree registry. The only worktree
knowledge is (a) git itself (`git worktree list`), (b) a host-side path
convention `<repos>/.worktrees/<repo>/<slug>` created by an operator-side
helper outside this repo, and (c) agents learning their worktree from the
prose of the spawn prompt. The gate hook's only worktree awareness is
ignoring `.worktrees` as a path segment when guessing a target repo.

## 3. Data model

New OPTIONAL item field (schema stays 2; readers ignore unknown keys, absent
means legacy):

```json
"worktree": {
  "repo":   "<repo-dir-name>",
  "branch": "<branch name>",
  "path":   "<abs worktree path, informational>",
  "source": "explicit|description|cwd"
}
```

Derived scope token, stored FIRST in `scope` and mirrored in
`worktree.token`:

    branch:<repo>@<branch>

Why a new `branch:` namespace rather than reusing `repo:<name>`:
`repo:` is validated as a directory name and means "the whole repo"; a
branch-qualified claim is strictly narrower. Why `@` rather than `/`:
branch names contain `/` (`user/feature`), and `path:` already splits on `/`.
The branch is stored verbatim (git refs cannot contain `@{` or spaces; reject
those in normalization). The token is a typed token (add `branch` to
`_TYPED_TOKEN_RE`).

Overlap rules for `branch:` (additions to `_tokens_overlap`):

* `branch:R@B` vs `branch:R@B` -> overlap; different B or R -> disjoint.
* `repo:R` vs `branch:R@*` -> OVERLAP (a whole-repo claim still wins, the same
  rule as `repo:` vs `path:`); this keeps legacy `repo:R` items and any
  deliberate whole-repo manual scope safe and serializing against every
  branch of R.
* `path:R/..` vs `branch:R@B` -> overlap (conservative: a path claim does not
  say which branch). Decision D4 may relax this.
* `*` overlaps everything (unchanged). Everything else exact.

## 4. Derivation rules

Resolved once, at `queue add` (and re-derivable at `register`), in priority order:

1. Explicit `--worktree PATH` (or `--branch NAME --repo NAME`): PATH must be a
   git worktree (`git -C PATH rev-parse --git-common-dir`, `--abbrev-ref HEAD`).
   `repo` = basename of the common dir's working tree owner (the repo dir
   under the repos root). Detached HEAD -> refuse unless `--branch` given.
2. Description/summary mentions EXACTLY ONE existing worktree path (reusing
   the unambiguous-single-target logic of the add-time heuristic). Zero or
   several -> no derivation (never guess). Prohibition-only mentions
   ("do not touch <path>") do not count.
3. cwd of the caller if it is inside a linked worktree (not the main clone).
   Off by default (`--worktree-from-cwd`) because the main loop's cwd is not
   the work target; useful for agents/CLI use.
4. Nothing resolves: item is path-less, legacy behaviour, `--scope` required
   exactly as today (see risks).

The main clone checkout itself is a worktree of its default branch; deriving
`branch:R@main` there is allowed but flagged (it is the shared-state path the
convention tells agents to avoid).

## 5. The "cannot be overridden" rule

Define the derived token as PROTECTED on the item (`worktree.token`).

* `queue add --worktree W --scope X`: `--scope` tokens are ADDED. The derived
  token is prepended. `--scope` is no longer required when a worktree
  resolves.
* `--scope repo:R` / `--scope branch:R@other` together with a worktree for R@B:
  a `repo:R` is accepted as an explicit additional whole-repo claim but
  prints a WARNING that it re-serializes all branches of R. A DIFFERENT
  `branch:` token for the same repo is REFUSED (exit 1): it would be an
  override attempt. A `branch:` for another repo is allowed (cross-repo item).
* `update-scope ID TOKEN...` (add): unchanged, additive. A `branch:` token
  that conflicts with the protected one is refused as above.
* `update-scope --remove <derived token>`: refused (exit 1) unless the item
  has no `worktree` field. Removing other tokens works.
* Re-pointing a mistaken derivation is a separate, explicit, audited verb
  (`queue rebind ID --worktree PATH`) that rewrites field + token atomically;
  there is no way to just edit the scope.
* Escape hatch: `QUEUE_GATE_BYPASS=1` / `--force-enqueue` already exist for
  gate bypass; reuse `--no-worktree-scope` ONLY at add time for the path-less
  case, recorded as `worktree: null, worktree_opt_out: true`.

## 6. Additive manual scopes

`--scope` keeps its name and meaning but becomes strictly additive when a
worktree exists. `hostjob:`, `resource:`, `tool:`, `cluster:`, `workload:`
(still subject to its existing manual-add refusal), `task:` are untouched by
the derivation and by validation. Only `repo:` tokens keep the repos-dir
validation; `branch:` validates that `<repo>` is a repos-dir name (same
fail-open) and does NOT require the branch to exist (a branch may be created
later or already deleted).

## 7. Backwards compatibility

* Existing items have no `worktree` key: loaded as-is, scopes unchanged, all
  current tests continue to pass untouched.
* Queue file schema stays 2. Old code reading a new file ignores `worktree`
  and sees `branch:` as an unknown-but-exact token: it still serializes
  same-branch items; it just never overlaps with `repo:R` (the one place a
  downgrade is less safe). Mitigation: dual-write period (section 9).
* Gate hook: it only reads `status` and `scope`. Its repo/target check looks
  at `repo:`/`path:` tokens; a scope of only `branch:R@B` would currently look
  like "no repo scope" and default-open. It must learn `branch:` -> repo `R`
  so the target-mismatch protection keeps working (small hook change; also a
  chance to check the prompt's worktree path against the item's recorded
  worktree and warn on mismatch).
* Arm hook: unchanged (binds q-id to agent id; no scope parsing).
* Exporters/minisite that print scopes see a new token shape only.
* `_extract_repo_paths_from_scope` (recovery bundle) maps `repo:`/`file:`;
  add `branch:` -> use `worktree.path` when present.

## 8. How serialization changes (examples)

Today (all scoped `repo:R`):

| Item | Branch | Today |
|---|---|---|
| Training-code fix | lane-A | serializes behind next two |
| Dashboards | lane-B | serializes |
| Checkpoint layout change | lane-C | serializes |

After: tokens `branch:R@lane-A`, `branch:R@lane-B`, `branch:R@lane-C` are
pairwise disjoint -> run in parallel. A second item on lane-A queues behind
the first. A manual `--scope hostjob:x` on two items of different branches
still serializes them on `hostjob:x` only. A legacy or manual `repo:R` item
still blocks and is blocked by all three (whole-repo wins).

## 9. Serialization changes in the file and migration

* No bulk rewrite. Phase 0: ship `branch:` token + overlap + gate-hook parsing
  (no producer yet; harmless). Phase 1: add `--worktree`, derivation, the
  protected-token rules, `rebind`; derivation OPT-IN via env
  `SESSION_TASK_DERIVE_WORKTREE_SCOPE=1`. Phase 2: default on, `--scope`
  still required when nothing resolves. Phase 3 (optional): warn on `repo:R`
  scope when the description names a worktree.
* In-flight items keep their old tokens until they finish (items are
  short-lived); no migration of queue.json is required.
* Deploy order matters: gate hook and CLI must both understand `branch:`
  BEFORE any producer emits it (the CLI and hook are bind-mounted and baked
  separately, see the redeploy docs; sync the source clone first).
* Rollback: remove the env flag; existing `branch:` items drain; nothing
  persistent to undo.

## 10. Risks

1. Path-less items (research, cross-repo, ops): no derivation; must remain
   possible via explicit `--scope` or `--no-worktree-scope`. Risk: operators
   drift back to `repo:R` for everything; mitigate with the warning in
   phase 3 and docs.
2. Two items intentionally sharing a worktree: same token, they serialize,
   which is the intent (and the desired answer to "why shared scope").
3. Worktree removed mid-item: token is just a string; item continues. The
   path is informational only; never stat it for overlap or readiness.
4. Branch rename / checkout switch inside the worktree: token goes stale and
   can split one lane in two. Mitigation: derive from branch at add time and
   store it; `rebind` for renames; optional register-time re-check that warns
   when `HEAD` no longer matches.
5. Detached HEAD / rebases: refuse or require `--branch`.
6. Monorepo vs service repos: derivation is per git repo, so a monorepo lane
   is one token and still over-serializes intra-branch; `path:` additional
   scopes remain the lever for narrowing within a branch (see D4).
7. Divergent worktrees of one branch: git forbids the same branch checked out
   twice, so one branch == one worktree; fine.
8. Description mis-detection picks the wrong worktree: mitigated by requiring
   exactly one match and printing the derived token loudly; `rebind` fixes.
9. Gate-hook/CLI version skew (section 9 deploy order).
10. Protected-token refusal may block a legitimate manual repair; `rebind` and
    the bypass env cover it.

## 11. Test plan

Follow the existing subprocess + temp-HOME style (new
`tools/session-task/tests/test_queue_worktree_scope.py`, plus hook tests next
to the existing hook tests).

* Derivation: explicit path, description-mentioned (one / zero / many /
  prohibition-only), cwd opt-in, detached HEAD, main clone, non-git dir,
  branch with slashes, token normalization and invalid branch characters.
* Overlap: `branch` same/different, vs `repo:` (overlap), vs `path:` (overlap),
  vs `*`, cross-repo disjoint; symmetric property test over a token matrix.
* Protected rule: add with conflicting `branch:` refused; `--remove` of the
  derived token refused; removing other tokens fine; `rebind` rewrites field
  and token atomically.
* Additive: `--scope hostjob:x resource:y` retained alongside the derived
  token; order stable; dedupe; comma-split parity.
* Serialization: three branches parallel; two same-branch serialize;
  `repo:R` legacy serializes against all; group merge via `hostjob:`.
* Back-compat: legacy fixture queue.json (no `worktree` key) loads, lists,
  spawn-checks, round-trips; all existing tests unchanged and green.
* Hook: gate allows spawn with only `branch:` scope, target-mismatch still
  works (`branch:R@B` vs prompt about another repo), recorded-worktree
  mismatch warning; arm hook unchanged.
* Concurrency: existing flock tests extended with branch-token adds.

## 12. Size estimate

| Area | Files | Rough lines |
|---|---|---|
| session-task: token + overlap + validation + derivation + flags + protected rules + rebind | 1 (`session-task`) | 300-400 |
| gate hook `branch:` parsing + worktree mismatch warn | 1 | 40-70 |
| docs (`docs/queue.md` token table, agent protocol text in baked policy doc if adopted) | 1-2 | 40-80 |
| tests | 2-3 new files | 450-650 |
| Total | ~6-7 files | ~850-1200, mostly tests; core logic ~400 |

Roughly 1.5-2.5 agent-days of work including review iterations. One PR is
feasible; splitting into Phase 0 and Phase 1 PRs is safer for deploy order.

## 13. Decisions needed (with a lean)

* D1. Token shape: `branch:<repo>@<branch>` (new namespace) vs reuse
  `repo:<repo>@<branch>`. Lean: `branch:`; keeps `repo:` validation and the
  whole-repo meaning intact.
* D2. Does `repo:R` overlap `branch:R@*`? Lean: yes (whole-repo claim wins),
  with a warning when added manually next to a derived token.
* D3. Conflicting `--scope branch:R@other` for the same repo: refuse vs
  silently additive. Lean: refuse (that is the "cannot override" rule);
  `repo:R` additional is allowed with a warning.
* D4. Should `path:R/..` overlap every branch of R? Lean: yes for v1
  (conservative); revisit once branch lanes are common.
* D5. Auto-detect from description: on by default or only explicit
  `--worktree`? Lean: explicit flag first (phase 1), auto-detect default in
  phase 2 with a loud printout of what was derived.
* D6. Path-less items: keep `--scope` required, or allow a `--no-worktree`
  opt-out recorded on the item? Lean: keep required plus an explicit opt-out
  flag that is recorded for audit.
* D7. Main-clone derivation (`branch:R@main`): allow, warn, or refuse? Lean:
  warn (the convention already discourages it) rather than refuse.
* D8. Rollout: env-flag opt-in then default-on (phased) vs immediate. Lean:
  phased, because the CLI and hook deploy separately.
* D9. Re-derive on `register` if HEAD moved? Lean: warn only, never rewrite;
  `rebind` is the sole mutator.
