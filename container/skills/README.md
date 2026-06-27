# container/skills/

Slash-command source files baked into the [claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container) image. Each file is one skill that the in-container `claude` process can invoke as `/<plugin>:<name>` (the plugin name is `claude-container`, set by `/opt/claude-container/plugin/.claude-plugin/plugin.json`).

## What goes here

- One Markdown file per skill: `<name>.md`.
- Skill bodies follow the same shape as the host's `~/.claude/commands/<name>.md` files: a one-line summary at the top, then `## Steps` / `## Important` / etc. The first line should be the skill's prompt-injection summary so the agent sees it in `--help`-style listings.
- No frontmatter is required (mirroring the host shape). If a skill needs metadata (description override, allowed-tools restriction), add a YAML frontmatter block at the top — Claude Code's plugin loader honours it.

## How they get baked in

The Dockerfile copies this directory into the image at two paths:

1. `/opt/claude-container/skills/` — canonical bake path; documented for operators who want to inspect what shipped with their image (e.g. `ls /opt/claude-container/skills/`).
2. `/opt/claude-container/plugin/commands/` — the path Claude Code's plugin loader actually reads. The Dockerfile populates this dir at build time and the entrypoint launches `claude` with `--plugin-dir /opt/claude-container/plugin`, so every baked skill becomes discoverable as `/claude-container:<name>` in the in-container session.

The two paths share contents (the Dockerfile copies `container/skills/` into both). Operators reading `/opt/claude-container/skills/` see the same files Claude Code actually loads.

## How a fresh container session discovers them

`entrypoint.sh` adds `--plugin-dir /opt/claude-container/plugin` to the `CLAUDE_CMD` it spawns under tmux. Claude Code's plugin loader walks `commands/` inside the plugin dir and registers each `<name>.md` as a slash command. Inside an interactive session the agent can verify discovery with:

```
/claude-container:claude-code-restart   # invoke the baked Claude-Code-restart skill
```

Listings: ask the agent "list available skills" — the plugin's commands show up with the `claude-container:` prefix.

## How to add a new skill

1. Drop `container/skills/<name>.md` in this dir. Match the existing tone — short, punchy, references in-container paths (not host paths).
2. (Optional) Add a test in `container/tests/` asserting the file exists at the baked path. The skeleton in [`container/tests/baked-dirs.test`](../tests/baked-dirs.test) already covers `claude-code-restart.md`, `restart-container.md`, and `start-watchers.md` — extend it for new skills.
3. Rebuild the image (`make compose-build` from the repo root, or `docker compose build claude-container` from `examples/compose/`).
4. `cwsr` the running container — wait, that won't pick up the new skill (it only re-execs claude with the same `--plugin-dir` arg pointing at the same already-baked files; the new files only land after a container rebuild). Recommend `docker compose up -d --force-recreate claude-container` instead.

## Test conventions

- Unit-style tests for skill files live alongside other container tests in [`container/tests/`](../tests/).
- The baseline test ([`container/tests/baked-dirs.test`](../tests/baked-dirs.test)) asserts every documented skill is non-empty and references the right backing tool (e.g. `claude-code-restart` references `cwsr`).
- A skill-discovery integration test ([`container/tests/skill-restart-discovery.test`](../tests/skill-restart-discovery.test)) exercises the `--plugin-dir` wiring against a synthetic input directory — no docker required.

## Currently shipping

- [`claude-code-restart.md`](claude-code-restart.md) — restart the in-container `claude` (Claude Code) process via `cwsr` (mirrors the host's `/restart` skill, which uses `claude-watch update --force` against the systemd daemon; the container variant rolls only the inner pane-0 process, NOT the container — for that see the sibling `restart-container` skill).
- [`restart-container.md`](restart-container.md) — restart the whole CONTAINER via `docker compose restart claude-container` (issued through host-bash): re-runs `entrypoint.sh` → `obligations-init` (RE-SEEDS obligations) and clears in-container process state, WITHOUT recreating from the image / picking up env / mount changes. Distinct from `claude-code-restart.md` (inner-process roll) and from force-recreate (`make deploy-container`, picks up image / env / mounts).
- [`start-watchers.md`](start-watchers.md) — discover and launch any baked container-scoped watchers (today: none; the dir is a stub for phase-2 watcher integrations).
- [`self-clear.md`](self-clear.md) — trigger a clean CONTEXT reset of the in-container Claude Code session via `self-clear`: inject `/clear` into pane 0, poll tmux until the clear completes, then inject a resume prompt — a PROGRAMMATIC context reset that doesn't wait on the daemon's resume-injection path. Distinct from `claude-code-restart.md` (rolls the inner `claude` binary) and `restart-container.md` (restarts the container) — self-clear changes neither, only the conversation context.
