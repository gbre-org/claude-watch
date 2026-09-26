---
name: authoring-skills
description: "Meta-skill for writing a NEW skill for this system — SKILL.md/frontmatter authoring basics, and the baked-vs-linked decision (in-tree container/skills/ needs an image rebuild; linked-skills is a hot runtime install via install-linked-skills.sh, no rebuild). Use when asked to 'add a skill', 'write a new skill', 'create a slash command', or 'teach the agent how to do X as a skill'."
---
Teaches how to add a new skill to this system: the SKILL.md authoring
basics, and — the part that trips people up — **which of the two delivery
mechanisms to use** and how to execute each. Read this before creating a
new skill, not after you've already picked (wrongly) between them.

## The two ways a skill reaches a container session

A skill file only helps if it's actually discoverable in the target
session. There are exactly two paths into a `claude-container` session,
and they trade off rebuild cost against permanence:

| | **Baked** (in-tree) | **Linked** (out-of-tree) |
| --- | --- | --- |
| Lives at | `container/skills/<name>.md` (this repo) | any repo's `.claude/skills/<name>/SKILL.md` (or a `skills/` tree) |
| Reaches the container via | `Dockerfile` `COPY` into `/opt/claude-container/plugin/commands/` | `scripts/install-linked-skills.sh` copies into a central host dir, bind-mounted RO into the container as a second `--plugin-dir` |
| Surfaces as | `/claude-container:<name>` | `/linked-skills:<name>` |
| To add/update | **image rebuild + container recreate** (`make deploy-container`) | **re-run the installer**; picked up on the *next session start* — no rebuild, no recreate |
| Pick this when | the skill drives the container's OWN lifecycle (recreate/restart/roll `claude`), edits bind-mounts/compose shape, or needs in-container-only paths (`/opt/claude-container/...`, the tmux pane, `host-bash`) | the skill is useful skill-content that lives naturally in its own repo (or the claude-watch repo root `skills/`) and would make sense on a machine with no container at all |

**Default to linked** unless the skill is specifically about the
container's own lifecycle — baked skills cost a rebuild for every edit;
linked skills are a copy + next-session-start.

This meta-skill you're reading right now is itself baked (it's about
*this repo's* skill-authoring convention, container-only in spirit since
it documents `container/skills/`) — but most new skill content should NOT
default to baked. When in doubt, ask: "would this still make sense on a
laptop with no claude-container running?" If yes, it's linked (or the
shared root `skills/` dir on a host deploy) — see
[`skills/README.md`](../../skills/README.md) vs
[`container/skills/README.md`](./README.md) for the full baked-vs-shared
split, which is orthogonal to (and BEFORE) the baked-vs-linked question
for anything container-adjacent.

## Full linked-skills reference — read it, don't duplicate it

The linked-skills mechanism (PR #798) is fully documented in
[`docs/linked-skills.md`](../../docs/linked-skills.md). Read that file for
the actual how-to; the essentials, so you know whether you need it:

- **Why it exists**: a host `~/.claude/skills/<name>` symlink doesn't
  resolve in-container (tmpfs `~/.claude` under the OAuth-neutralization
  farm, and host-absolute symlink targets don't exist in the container's
  filesystem namespace) — so skills from other repos get **copied**, not
  symlinked, into a central plugin dir.
- **Mechanism**: `scripts/install-linked-skills.sh --src <dir>` (or `make
  install-linked-skills SRC=<dir>`) copies each `<name>/SKILL.md` (+ a
  `.provenance.json` stamped with source repo/commit/timestamp) into
  `~/.config/claude-container/linked-skills/skills/`, which is bind-mounted
  RO to `/opt/claude-container/linked-skills` via
  `CLAUDE_HOST_LINKED_SKILLS_DIR`. `container/entrypoint.sh` adds it as a
  **second `--plugin-dir`** (after the baked one), guarded by a
  `.claude-plugin` existence check.
- **Why `--plugin-dir` and not `--add-dir`**: `--add-dir` skill loading
  needs the `project` setting-source, which the container's OAuth-farm
  launch mode drops (`--setting-sources user,local`). `--plugin-dir` is
  setting-source-independent — the same proven mechanism baked skills
  already use.
- **What it costs to add/update a skill once the mount exists**: just
  re-run the installer. **No image rebuild, no `make deploy-container`.**
  The next in-container session start picks it up.
- **First-time enable** (if the bind-mount doesn't exist yet on a given
  deployment) is a one-time `CLAUDE_HOST_LINKED_SKILLS_DIR` env wire-up +
  one recreate — after that, every future skill add/update is rebuild-free.

## Authoring the SKILL.md itself

Whichever mechanism you picked, the file shape is the same — Claude
Code's plugin loader reads one Markdown file (baked) or one
`<name>/SKILL.md` directory (linked) per skill:

1. **Frontmatter** (YAML, `---`-fenced) at the top:
   ```yaml
   ---
   name: my-new-skill
   description: "One sentence: what it does + WHEN to use it, packed with the trigger words a user or agent would actually type."
   ---
   ```
   The `description` is what a listing / discoverability pass matches
   against — front-load concrete trigger phrases ("add a skill", "restart
   the container", "check CI"), not just an abstract summary. This file's
   own frontmatter above is a worked example: it names the literal phrases
   ("add a skill", "write a new skill", "create a slash command") someone
   would type to reach it.

2. **First body line** is the prompt-injection summary — write it as a
   complete, standalone sentence; some listing views show only this line.

3. **Body structure** — mirror existing skills' shape (`## Steps`, `##
   Important`, `## When NOT to use this`), not prose paragraphs. Look at
   [`self-clear.md`](./self-clear.md) in this dir for a fully-worked
   example: one-line summary, a "yes you can do X" framing where the skill
   corrects a common wrong assumption, numbered `## Steps`, a "when this is
   NOT the right tool" section that cross-references sibling skills, and an
   `## Important` section for the boring-but-load-bearing facts (binary
   path, source link, lock-file behavior).

4. **Keep it lean.** A skill is a prompt injection, not a design doc — link
   out to `docs/*.md` for anything with real depth (this file links to
   `docs/linked-skills.md` rather than re-explaining the mechanism's
   internals). Every extra paragraph is context budget spent on every
   session that triggers the skill, whether or not it needed that detail.

## If you're adding a BAKED skill specifically

1. Confirm it belongs here and not in the shared root `skills/` dir — see
   the table in [`container/skills/README.md`](./README.md).
2. Drop `container/skills/<name>.md` in this dir, matching the tone/shape
   above.
3. Optionally extend [`container/tests/baked-dirs.test`](../tests/baked-dirs.test)
   to assert the new file exists and hits its key facts.
4. **Rebuild the image** (`make compose-build` or `docker compose build
   claude-container`), then **force-recreate**
   (`make deploy-container` / `docker compose up -d --force-recreate
   claude-container`). `cwsr` alone does NOT pick up a new baked skill — it
   only re-execs `claude` against the same already-baked `--plugin-dir`
   contents.
5. Watch [`container/baked-CLAUDE.md`](../baked-CLAUDE.md)'s hard
   74,900-char size ceiling (`scripts/check-claude-md-size.py`) if your
   change touches that file at all — a new skill dir under
   `container/skills/` normally shouldn't need to.

## If you're adding a LINKED skill

1. Write `<name>/SKILL.md` in the skill's own home repo (e.g.
   `.claude/skills/<name>/SKILL.md` or a `skills/` tree) — same frontmatter
   + body conventions as above.
2. Run the installer against that repo:
   ```sh
   scripts/install-linked-skills.sh --src <path-to-repo-or-skills-dir>
   # or: make install-linked-skills SRC=<path>
   ```
3. Confirm the central dir picked it up (`~/.config/claude-container/linked-skills/skills/<name>/`)
   and that the bind-mount (`CLAUDE_HOST_LINKED_SKILLS_DIR`) is wired for
   the target deployment — one-time per deployment, see
   [`docs/linked-skills.md`](../../docs/linked-skills.md#what-needs-to-happen-for-a-change-to-go-live).
4. Next container session start: verify with `/linked-skills:<name>` (or
   ask the agent to list available skills — it shows up with the
   `linked-skills:` prefix). No rebuild, no recreate needed if the mount
   already exists.

## Quick decision checklist

- Does it manage the container's own recreate/restart/roll, bind-mounts,
  or compose shape, or need `/opt/claude-container/...` / the tmux pane /
  `host-bash`? → **baked**, `container/skills/<name>.md`, rebuild+recreate
  to ship.
- Would it make sense with no container running at all, or does it
  naturally live in another repo? → **linked** (or the shared root
  `skills/` for claude-watch-owned deployment-agnostic skills), installer
  re-run + next session start to ship, no rebuild.
