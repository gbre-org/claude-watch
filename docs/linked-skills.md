# Linked-in skills — loading repo skills inside claude-container

Repo-linked **Agent Skills** (a `SKILL.md` directory, e.g. `eichi-search`) that
a host operator installs into `~/.claude/skills/` do **not** load inside
claude-container. This doc explains why, and the mechanism that fixes it.

## Why host `~/.claude/skills` doesn't carry through

Two independent reasons, either of which alone breaks it:

1. **The in-container `~/.claude` is often a fresh tmpfs.** When the
   OAuth-neutralization farm is active (`CW_NEUTRALIZE_HOME_CLAUDE_OAUTH=1`),
   `/home/hndrewaall/.claude` is a tmpfs farm, not the host mount — so
   `~/.claude/skills` never appears in-container at all.

2. **Host skill symlinks are host-absolute.** A `~/.claude/skills/<name>` entry
   is typically a symlink whose target is a HOST path
   (`/Users/hallandrew/repos/.worktrees/…`). That path does not exist in the
   Linux container's filesystem namespace, so even mounted through a bind-mount
   the symlink dangles.

**Verified symlink-through-bind-mount test** (2026-09-24, inside the container,
reading symlinks created on the host in a bind-mounted dir):

| Symlink target style | Resolves in-container? |
| --- | --- |
| host-absolute (`/Users/hallandrew/…`, the `ln -s` default) | **NO** — target absent in container namespace |
| container-absolute (`/home/hndrewaall/…`) | yes |
| relative (`./real-target-dir`) | yes |

The natural host workflow produces host-absolute targets, so **symlinks are not
reliable across the boundary → we COPY** (the locked design's fallback path).

## The mechanism

A single **central dir** — `~/.config/claude-container/linked-skills` on the
host — is bind-mounted (read-only) over the image path
`/opt/claude-container/linked-skills`. That dir is a Claude Code **plugin**:

```
linked-skills/
  .claude-plugin/plugin.json     # manifest (name: linked-skills); seeded by the installer
  skills/
    eichi-search/
      SKILL.md                   # the skill payload, COPIED from source
      .provenance.json           # source repo + path + commit + install timestamp
```

`container/entrypoint.sh` appends a **second `--plugin-dir`** for it, right
after the baked `/opt/claude-container/plugin` one, guarded by
`[ -d /opt/claude-container/linked-skills/.claude-plugin ]`. Installed skills
then surface in-container as `/linked-skills:<name>`.

### Why `--plugin-dir`, not `--add-dir`

Claude Code's documented "extra skills dir" flag is `--add-dir` (it loads
`<dir>/.claude/skills/`). But `--add-dir` skill loading depends on the
**`project` setting-source**, which the container's OAuth-farm launch mode drops
(it launches `--setting-sources user,local`). `--plugin-dir` is
setting-source-independent and is already the proven baked-skill mechanism, so
it works in **every** launch mode. Verified: a headless
`claude -p … --plugin-dir <dir>` surfaced a `skills/<name>/SKILL.md` skill as
`/<plugin>:<name>`.

## Populating the central dir — `make install-linked-skills`

```sh
# From a repo that ships skills under .claude/skills/ (or a skills/ dir):
make install-linked-skills SRC=~/repos/eichi
# or point at any skills tree / single-skill parent:
scripts/install-linked-skills.sh --src <dir> [--dest <dir>] [-n] [--prune]
```

- **COPIES** each `<name>/SKILL.md` dir into `<dest>/skills/<name>/`.
- **Stamps provenance** (`.provenance.json`: `source_repo`, `source_path`,
  `source_commit` — with `-dirty` suffix if the source tree is dirty —
  `installed_at`, `installed_by`) so every skill in the central dir is traceable
  and stale installs are visible. `--prune` drops installs whose source path is
  gone.
- **Seeds** `<dest>/.claude-plugin/plugin.json` on first run.
- `DEST` defaults to `$CLAUDE_LINKED_SKILLS_DIR`, else
  `~/.config/claude-container/linked-skills`.

Adding a skill is a **runtime op**: re-run the installer into the central dir.
No docker rebuild, no compose edit.

## What needs to happen for a change to go live

| Change | To take effect |
| --- | --- |
| **First-time enable** (wire the bind-mount) | set `CLAUDE_HOST_LINKED_SKILLS_DIR` (or the override compose) + one container **recreate** (`make deploy-container`) |
| **Add / update a skill** after the mount exists | re-run `make install-linked-skills`; picked up on the **next in-container session start** (no recreate) |
| **entrypoint.sh / Dockerfile change** (this feature's wiring) | image **rebuild** + recreate (`entrypoint.sh` is baked, `COPY`+`ENTRYPOINT`) |

The default `CLAUDE_HOST_LINKED_SKILLS_DIR=/dev/null` is a graceful no-op:
docker mounts the null device, `entrypoint.sh`'s `.claude-plugin` test is false,
and no `--plugin-dir` is added — same shape as the managed-settings default.

## Files

- `scripts/install-linked-skills.sh` — the COPY + provenance installer.
- `container/entrypoint.sh` — second `--plugin-dir` block (consecutive-if
  contract; comments stay inside the `if` body).
- `examples/compose/docker-compose.yml` — the `CLAUDE_HOST_LINKED_SKILLS_DIR`
  bind-mount over `/opt/claude-container/linked-skills`.
- `Makefile` — `install-linked-skills` target.
