# skills/

Slash-command source files that work in **any** claude-watch deployment — host
(systemd) *and* container. One Markdown file per skill: `<name>.md`.

Skills that only make sense **inside** the container live in
[`container/skills/`](../container/skills/) instead. See
[the split](#the-split-shared-vs-container-only) below.

## Where they end up

| Deployment | How they get there | Invoked as |
| --- | --- | --- |
| **Host / systemd** | `make install-skills` (a dependency of `make deploy-systemd`) symlinks each `<name>.md` into the Claude Code commands dir as `cw-<name>.md` | `/cw-<name>` |
| **Container** | `container/Dockerfile` COPYs this dir into `/opt/claude-container/skills/` and `/opt/claude-container/plugin/commands/`, alongside `container/skills/` | `/claude-container:<name>` |

### Everything from this repo is prefixed

In both modes a skill that came from claude-watch is visibly namespaced, so
its origin is obvious at the prompt and it cannot silently shadow a skill the
operator wrote themselves:

- container: the `claude-container` **plugin namespace**, set by
  [`container/plugin/.claude-plugin/plugin.json`](../container/plugin/.claude-plugin/plugin.json)
  and loaded via `--plugin-dir`.
- host: a `cw-` **filename prefix**. Claude Code derives a user-level slash
  command's name from the filename, so a prefix is the host analogue of a
  plugin namespace — with no manifest to maintain and no change to how the
  operator launches `claude`. (On a host deploy this repo's Makefile does not
  control the `claude` argv, so `--plugin-dir` is not available to it.)

## The split: shared vs container-only

A skill belongs **here** if it would still make sense on a machine with no
container at all. It belongs in [`container/skills/`](../container/skills/) if
it does any of:

- drives the container's own lifecycle (recreate it, restart it, roll the
  inner `claude` process);
- edits the container's bind-mounts / compose shape;
- depends on in-container-only paths (`/opt/claude-container/...`), the
  in-container tmux pane, or the `host-bash` MCP bridge.

Those are exactly the skills that would be dead weight — or actively
misleading — as `/cw-*` commands on a host, so `install-skills` never installs
them.

## Host install: what it will and will not touch

`scripts/install-host-skills.sh` (run it directly with `-n` for a dry run)
writes into a directory that is usually **managed by someone else** — on a
typical host `~/.claude/commands` is a symlink into the operator's own private
dotfiles repo holding dozens of hand-written skills. So the installer owns
exactly the paths it created:

- installs each skill as an **absolute-path symlink** back into this repo, so
  editing a skill in-tree is live immediately with no reinstall (the same
  policy `make install` uses for scripts);
- **never** overwrites a regular file, or a symlink pointing outside this
  `skills/` dir — it reports the clash on stderr and carries on, so one
  operator-owned filename can't block a deploy;
- **only** prunes `cw-*.md` symlinks that point into this `skills/` dir and
  whose target is gone (a skill renamed or deleted upstream);
- is idempotent — re-running re-asserts the same links.

Override the destination with `CLAUDE_COMMANDS_DIR` or `--dest DIR`, and the
prefix with `--prefix`.

## Adding a skill

1. Decide shared (here) vs container-only (`container/skills/`) using the
   split above.
2. Drop `<name>.md` in the chosen dir. First line is the prompt-injection
   summary that shows up in listings; then `## Steps` / `## Important` /
   `## When NOT to use`. Frontmatter is optional (Claude Code honours
   `description`, `argument-hint`, `allowed-tools`, `model` if present).
3. **Public repo — write it standalone.** No private paths, no
   operator-specific repo locations, no quoted private notes. Describe the
   pattern generically; a reader with a fresh checkout must be able to follow
   it. Guarded by `make test-install-host-skills`.
4. `make test-install-host-skills` (and `make test-entrypoint` if you touched
   `container/skills/`).
5. Host: it goes live on the next `make deploy-systemd`, or immediately with
   `make install-skills`. Container: needs a rebuild + force-recreate — `cwsr`
   will NOT pick it up.

## Currently shipping

- [`distill.md`](distill.md) — the DISTILLATION METASKILL: take a completed
  piece of work (a transcript, finished session, or repeated agent pattern) and
  systematically distill it into a reusable artifact via IDENTIFY → CHOOSE →
  DRAFT → PLACE. Adds the decision-tree for artifact type (skill / agent-prompt
  / CLI tool / memory), format, and placement.
- [`pr-comment-triage.md`](pr-comment-triage.md) — triage the accumulated bot +
  human comments on recently-updated tracked PRs: classify each comment by
  CONTENT (never author), collapse bare-status bot noise via `minimizeComment`
  GraphQL (OUTDATED, reversible), keep real signal visible, and DRAFT (never
  auto-post) human replies. The first worked instance of `distill.md`.
- [`setup-hooks.md`](setup-hooks.md) — install / uninstall the Claude Code
  hooks that let the claude-watch daemon fire conversational reminders before
  falling back to tmux injection.
