---
name: new-worktree
description: "Create a git worktree the CANONICAL way — always under <workspace>/.worktrees/<repo>/<slug>, with the approved git identity stamped at creation. Use INSTEAD OF a bare `git worktree add`, which an obligations gate now DENIES for non-canonical target paths."
---
Create a host-side git worktree the ONE sanctioned way, so worktrees never scatter across the workspace again.

**Use this instead of a bare `git worktree add`.** An AST-aware obligations gate (`worktree_canonical_path`) now DENIES any `git worktree add` whose target is NOT under the canonical root — including evasions after `&&` / `;` / `|`, inside `$(...)`, or behind `bash -c` / `env` wrappers. This skill is the discoverable front door to the right command.

## The canonical convention

ALL host git worktrees live under a single hidden root:

```
<workspace>/.worktrees/<repo>/<slug>
```

e.g. `~/repos/.worktrees/claude-watch/my-fix`, `~/repos/.worktrees/regrello/rc`. NEVER:

- a **nested** `<repo>/.worktrees/...` (worst case — that path is INSIDE the read-only bind-mounted clone),
- a loose `<workspace>/<repo>-worktrees/` container dir,
- a loose `<workspace>/.wt-*` dir,
- a top-level `<workspace>/<repo>-<name>/` (that looks like a separate clone, not a worktree).

## How to create one

Worktrees + git are **host-side** (`~/repos` is read-only in-container), so run these via the `host-bash` MCP bridge.

**Preferred — the identity-stamping helper** (creates the canonical path AND stamps the approved `user.name`/`user.email` deterministically, so the global git-identity hook never trips on a fresh worktree):

```sh
<path-to-your-worktree-helper> <repo-path> <slug> [branch]
# e.g. new-worktree.sh /Users/<you>/repos/claude-watch my-fix ah/my-fix
# prints the created worktree's absolute path on success
```

The operator keeps this helper in their own private config repo (it encodes the operator's approved-identity policy, which must not ship in a public repo). If you don't know its path, ask the operator or check the host-bash conventions doc — do NOT reimplement identity stamping inline.

**Manual equivalent** (no identity stamping — only when the helper is unavailable, and only for a repo whose clone already resolves the correct identity):

```sh
git -C <workspace>/<repo> worktree add <workspace>/.worktrees/<repo>/<slug> -b <branch>
```

Point every subsequent git/edit command at the worktree with `git -C <worktree>`.

## Removing a worktree when done

```sh
git -C <workspace>/<repo> worktree remove <workspace>/.worktrees/<repo>/<slug>
git -C <workspace>/<repo> worktree prune      # tidy stale admin entries
```

`worktree remove` and `worktree list` are NOT gated — only `worktree add` to a non-canonical path is.

## Notes

- Builds/deploys must run from the durable build worktree `<workspace>/.worktrees/<repo>/main` (kept on `origin/main`), never the operator's routinely-dirty main clone.
- Git shares the main clone's object store across worktrees and enforces one-checkout-per-branch, so the main clone and each worktree must be on different branches.
- If the gate DENIES a genuinely-intentional non-canonical add, bypass deliberately with `obligations override "<reason>" --duration <N>` — don't work around it by disabling the gate.
