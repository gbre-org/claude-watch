Restart the whole CONTAINER (not just the inner Claude Code process) via `docker compose restart claude-container`, issued through the `host-bash` MCP bridge. This restarts the container's process tree — re-running `entrypoint.sh`, which re-runs `obligations-init` (RE-SEEDS the baked + bind-mounted obligation rows) and clears in-container process state — WITHOUT recreating the container from its image. It is lighter than a force-recreate: it does NOT pick up a rebuilt image, changed entrypoint-time env vars, or new bind-mounts.

**SESSION-KILLING.** The restart kills PID 1 (process-compose) and with it the inner `claude` process; the current Claude Code session ends. The next session resumes automatically via `CLAUDE_AUTO_CONTINUE` (the entrypoint re-appends `--continue <value>` to the in-container claude invocation, so the prior conversation is picked back up).

## When to use this vs. the two siblings

There are three distinct "restart" operations. Pick by what you need re-run:

- **`/claude-container:claude-code-restart`** (a.k.a. `/claude-container:restart` pre-PR-#444; backed by `cwsr`) — rolls ONLY the inner `claude` binary in pane 0. Does NOT re-run `entrypoint.sh`, does NOT re-seed obligations, does NOT touch the container. Use it for "pick up a new Claude Code version / restart the inner process" and nothing more.
- **THIS skill — `/claude-container:restart-container`** (`docker compose restart claude-container`) — restarts the container's process tree, re-running `entrypoint.sh` → `obligations-init` (re-seeds obligations) and clearing in-container process state. Does NOT pick up image / entrypoint-env / mount changes. Use it when obligation rows got into a bad state, a baked-but-not-bind-mounted helper needs its setup re-run, or the in-container process tree needs a clean restart — but the image and the compose shape are unchanged.
- **force-recreate** (`docker compose up -d --force-recreate claude-container`, i.e. `make deploy-container`) — full recreate FROM the image; the ONLY one of the three that picks up a rebuilt image, changed entrypoint-time env vars (`CLAUDE_AUTO_CONTINUE`, `CLAUDE_CONTAINER_REWRITE_HOOKS`, `CLAUDE_HOST_PROJECT_DIR`, …), or new / changed bind-mounts. Use it after `make container-build` or any compose / env change. (Not this skill — referenced for contrast.)

Re-seed behavior is EMPIRICALLY CONFIRMED: a `docker compose restart` was observed to re-run the entrypoint and restore seeded obligation rows that had been removed. So "restart re-seeds obligations" is verified, not assumed.

## Steps

1. **Issue the restart through `host-bash`** (the in-container session has no host `docker`; `docker compose restart` runs on the HOST). A single command — the HOST docker daemon owns the operation and carries it to completion even after the issuing session dies, so there is NO `&` / `nohup` / `disown`, and it is NOT split into stop-then-start.

2. **Compose-file discovery — the load-bearing gotcha.** The deploy wires `COMPOSE_FILE` at the operator's config-dir override (`~/.config/claude-container/docker-compose.override.yml`), so the base + override merge is location-independent. A BARE `docker compose restart claude-container` from the wrong cwd fails with **"no configuration file provided: not found"** — docker only auto-discovers a compose file named `docker-compose.yml` in the current directory, and the gitignored override never lives in a worktree. Two robust ways to avoid this:

   - **Preferred (mirrors `make deploy-container`):** point `COMPOSE_FILE` at the base + config-dir override and restart the service by its compose name. Via `host-bash` `run_script` (so the `:`-joined `COMPOSE_FILE` and the conditional are handled verbatim):

     ```bash
     base=/Users/hallandrew/repos/claude-watch/examples/compose/docker-compose.yml
     override=$HOME/.config/claude-container/docker-compose.override.yml
     if [ -f "$override" ]; then
       COMPOSE_FILE="$base:$override" docker compose restart claude-container
     else
       COMPOSE_FILE="$base" docker compose restart claude-container
     fi
     ```

   - **Robust fallback — restart by CONTAINER NAME (no compose file needed at all):**

     ```
     docker restart compose-claude-container-1
     ```

     `docker restart <name>` (note: `docker restart`, NOT `docker compose restart`) needs no compose file because it addresses the container directly by name. The compose project name is `compose` (the `examples/compose/` dir) and the service is `claude-container`, so the container is `compose-claude-container-1`. Confirm the exact name first with `docker ps --format "{{.Names}}"` if unsure. This is the simplest path and immune to the compose-discovery failure — a sibling main loop hit exactly the "no configuration file provided" error doing the bare `docker compose restart`, so prefer the container-name form unless you specifically need the override merged (a plain `restart` doesn't re-read mounts anyway, so the override merge buys nothing here — the container-name form is the right default).

3. **Confirm + hand off.** Tell the operator the container restart was issued; the current session will die and a fresh one resumes via `CLAUDE_AUTO_CONTINUE`. The fresh session re-runs the session-start checklist (the entrypoint re-ran `obligations-init`, so the presence-gate / other manifest obligations are freshly seeded).

## Important

- Restart is NOT recreate. `docker compose restart` / `docker restart` reuse the SAME container — same image, same env, same mounts. Only the process tree restarts (and the entrypoint re-runs). For image / env-var / mount changes you MUST force-recreate (`make deploy-container`); restart will silently keep the old shape.
- Issue it via `host-bash` — the in-container shell has no `docker`. Frame it honestly as "ran `docker (compose) restart` on the host via host-bash", not "I restarted the container" — host-bash is a window to the host daemon.
- `host-bash` `run_command` splits on shell operators even inside quotes, so the `COMPOSE_FILE="$base:$override"` form (a `:` in a quoted value) belongs in `run_script`, not `run_command`. The plain `docker restart compose-claude-container-1` fallback is a single operator-free command and runs fine via either tool.
