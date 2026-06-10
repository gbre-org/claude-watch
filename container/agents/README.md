# container/agents/

Agent prompt source files baked into the [claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container) image. Each file describes one custom agent the in-container `claude` process can spawn via the `Agent` tool (e.g. `Agent(subagent_type="claude-container:explore", ...)`).

## What goes here

- One Markdown file per agent: `<name>.md`.
- Agent bodies follow the standard Claude Code agent shape: a YAML frontmatter block (`description: ...`, optional `allowed-tools: [...]`) followed by the system-prompt body the agent runs with. See the [Claude Code agent docs](https://code.claude.com/docs/en/agents) for the canonical schema.
- No agents currently ship. This dir exists as a stub for future agent ports (see "Future plan" below).

## How they get baked in

The Dockerfile copies this directory into the image at two paths:

1. `/opt/claude-container/agents/` — canonical bake path; documented for operators who want to inspect what shipped.
2. `/opt/claude-container/plugin/agents/` — the path Claude Code's plugin loader actually reads. The entrypoint launches `claude` with `--plugin-dir /opt/claude-container/plugin`, so every baked agent becomes discoverable as `claude-container:<name>` in the `Agent` tool's `subagent_type` parameter.

## How a fresh container session discovers them

Same mechanism as skills: `--plugin-dir /opt/claude-container/plugin` (added to `CLAUDE_CMD` by `entrypoint.sh`) tells Claude Code to walk `agents/` inside the plugin dir and register each `<name>.md` as a custom agent. Inside an interactive session the agent can verify discovery by asking "list the agents you can see" — the plugin's agents appear with the `claude-container:` prefix.

## How to add a new agent

1. Drop `container/agents/<name>.md` in this dir with proper frontmatter:

   ```markdown
   ---
   description: One-line agent purpose (shows up in agent listings)
   ---
   You are a focused agent for ...
   ```

2. (Optional) Add a test in `container/tests/baked-dirs.test` asserting the file exists at the baked path.
3. Rebuild the image (`make compose-build` or `docker compose build claude-container`).
4. `docker compose up -d --force-recreate claude-container` to pick up the new agent (a `cwsr` re-exec uses the same already-baked plugin dir; rebuild is required to ship new content).

## Future plan

Concrete container-scoped agent ports being considered for phase-2:

- **Explore** — fast read-only code-search agent. The host version is general-purpose; a container variant would be scoped to `${CLAUDE_HOST_PROJECT_DIR}` and the bind-mounted `~/repos/`.
- **general-purpose** — multi-step research / search inside container scope.
- **note-writer** — Obsidian-style notes, but writing into a container-bind-mounted scratch dir instead of the host's vault.

These would land as separate PRs once the use-cases firm up. Listing them here so contributors know the convention is "port useful host-side agents that make sense in container scope, drop them here, no other wiring needed".

## Test conventions

- Same as `container/skills/`: tests live in [`container/tests/`](../tests/). The baseline [`container/tests/baked-dirs.test`](../tests/baked-dirs.test) asserts this dir's README exists; extend it as agents land.
