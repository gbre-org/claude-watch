Redeploy the whole container with a FULL force-recreate FROM the image — `make deploy-container` (a single `docker compose up -d --force-recreate claude-container`), issued through the `host-bash` MCP bridge. This is the ONLY one of the three "restart" operations that picks up a REBUILT IMAGE, changed entrypoint-time env vars, or new / changed bind-mounts. Use it after `make container-build` or any compose / env / mount change.

**NEVER trigger a redeploy via `/exit`, `cw --clear`, `self-clear`, or ANY session-context-clearing operation.** Those clear or KILL the SESSION — they do NOT recreate the container from the image, so none of them pick up a rebuilt image, a changed entrypoint-time env var, or a new / changed mount. Worse, `/exit` in particular can be fired by the claude-code auto-upgrader, and an `/exit`-driven recreate can boot-loop on `~/.local/bin/claude: not found` if the versions volume isn't yet populated. The ONLY correct redeploy path is `make deploy-container` issued via the `host-bash` MCP bridge — the HOST docker daemon owns the recreate. (This skill is the redeploy path; `/exit` / `cw --clear` / `self-clear` are NOT.)

**SESSION-KILLING.** The recreate stops the old container and starts a fresh one from the image; the current Claude Code session ends with the old container. The next session resumes automatically via `CLAUDE_AUTO_CONTINUE` (the fresh entrypoint re-appends `--continue <value>`, so the prior conversation is picked back up). The single `up -d --force-recreate` is deliberately ONE host-daemon operation so it is safe to issue FROM INSIDE the container (self-redeploy): the HOST docker daemon owns the recreate and carries it to completion even after the issuing container (and the shell that ran `make deploy-container`) is torn down — so there is NO `&` / `nohup` / `disown`, and it is NOT split into a `down && up` (the second half would never run once the issuing container dies).

**`docker` is NOT in the container — this MUST run HOST-SIDE.** Running `make deploy-container` inline in the in-container `Bash` tool fails with `docker: not found` (the in-container shell has no host `docker`). Issue it through the `host-bash` MCP bridge (`run_script`), or have the operator run it from their host shell. This is the exact mistake that motivated baking this skill — a main loop tried `make deploy-container` in-container and got `docker: not found`. Frame it honestly as "ran `make deploy-container` on the host via host-bash", not "I redeployed the container" — host-bash is a window to the host daemon.

## When to use this vs. the two restart siblings

There are three distinct operations. Pick by what you need to pick up:

- **`/claude-container:claude-code-restart`** (backed by `cwsr`) — rolls ONLY the inner `claude` binary in pane 0. Does NOT re-run `entrypoint.sh`, does NOT re-seed obligations, does NOT touch the container. Use it for "pick up a new Claude Code version / restart the inner process" and nothing more.
- **`/claude-container:restart-container`** (`docker compose restart claude-container`) — restarts the container's process tree, re-running `entrypoint.sh` → `obligations-init` (re-seeds obligations) and clearing in-container process state. Does NOT pick up image / entrypoint-env / mount changes (it reuses the SAME container — same image, same env, same mounts). Use it when obligation rows got into a bad state or the in-container process tree needs a clean restart, but the image and compose shape are unchanged.
- **THIS skill — full force-recreate (`make deploy-container`)** — a full recreate FROM the image. The ONLY one of the three that picks up a REBUILT IMAGE, changed entrypoint-time env vars (`CLAUDE_AUTO_CONTINUE`, `CLAUDE_CONTAINER_REWRITE_HOOKS`, `CLAUDE_HOST_PROJECT_DIR`, …), or NEW / CHANGED bind-mounts. Use it after `make container-build` (a daemon / Dockerfile / baked-file change) or after any compose / env / mount edit. A plain restart would silently keep the OLD shape — for image / env-var / mount changes you MUST force-recreate.

## What `make deploy-container` actually does (read from the Makefile — don't guess)

`deploy-container` depends on `container-build`, then runs a SINGLE `docker compose up -d --force-recreate claude-container`. Concretely (in `examples/compose/`):

1. Runs `bin/prepare-host-claude-state` if present (mirrors `cw --up`): on macOS it bridges the Keychain Claude token into the dir-mounted `~/.claude/.credentials.json` (fail-closed — a locked keychain aborts the deploy so it never recreates into a logged-out container) and one-time-seeds the container-only `~/.claude.json`. Clean no-op on Linux and when run from INSIDE the container. It never tears down the running container, so the recipe shell survives to issue the atomic recreate.
2. **`COMPOSE_FILE` = base + config-dir override.** `COMPOSE_BASE` is this clone's `examples/compose/docker-compose.yml`; `COMPOSE_OVERRIDE` is `$HOME/.config/claude-container/docker-compose.override.yml` (the operator's personal bind-mounts — gh token, gitconfig, ssh-agent, Dropbox, ci-logs, clipboard bridge, cron.d, etc.). The override is appended to `COMPOSE_FILE` only if it exists (a fresh host with no override still recreates cleanly, base-only). Making the merge point at the config-dir override is what makes it LOCATION-INDEPENDENT — see the gotcha below.
3. **`--env-file` = config-dir `deploy.env`** if `$HOME/.config/claude-container/deploy.env` exists (deploy-critical vars like `CLAUDE_HOST_MANAGED_SETTINGS_DIR` — same worktree-invisibility class of bug as the override).
4. `docker compose … up -d --force-recreate claude-container`. Named volumes survive (no `-v`), so claude state / versions / the tmux socket dir persist across the redeploy.

`make redeploy` is a DEPRECATED ALIAS of `deploy-container` (kept working for the baked image's own scripts). `make container-build` and `make sync-main-clone` are separate targets (below); `deploy-container` depends on `container-build` but NOT on `sync-main-clone` (syncing the operator's working clone on every deploy is a bigger decision — kept explicit + opt-in).

## The full redeploy sequence (recurring-recreate-recovery playbook)

Run each step HOST-SIDE via `host-bash`. Paths are HOST paths (`/Users/hallandrew/...`).

a. **Refresh the durable build worktree** `~/repos/.worktrees/claude-watch/main` to `origin/main` (this is build-scratch — hard-reset is fine; never hold real work here; do NOT build from the operator's main clone, which is routinely dirty / behind):

   ```bash
   git -C /Users/hallandrew/repos/.worktrees/claude-watch/main fetch origin
   git -C /Users/hallandrew/repos/.worktrees/claude-watch/main reset --hard origin/main
   ```

b. **`make container-build` — ONLY if the image changed** (a daemon / Dockerfile / baked-file / skill / watcher change; SKIP for pure bind-mounted-CLI changes to `tools/obligations/*` etc., which need only the main-clone sync in step c). The build is LONG and blows past the host-bash 30s cap — drive it via `hostjob`:

   ```bash
   hostjob run --label cw-build --cwd /Users/hallandrew/repos/.worktrees/claude-watch/main -- make container-build
   hostjob wait cw-build   # blocks <=25s then exits 75 if still running — re-invoke on 75 until "done rc=0"
   ```

c. **Sync the operator MAIN CLONE `~/repos/claude-watch` to `origin/main`** — REQUIRED so bind-mounted fixes go live. The compose bind-mount `${HOME}/repos/ → /home/hndrewaall/repos/ (ro)` mounts the operator's MAIN CLONE, NOT the build worktree, and the in-container `obligations` / `session-task` CLIs (+ `tools/obligations/*`) resolve from it via PATH BEFORE the baked `/usr/local/bin` copy. A stale main clone SHADOWS merged+baked Python-CLI fixes and `make deploy-container` alone won't activate them (the recreate just re-mounts the same stale clone). Use the convenience target (ff-only, refuses rather than clobbering divergent local work) or the explicit fetch+merge:

   ```bash
   make -C /Users/hallandrew/repos/claude-watch sync-main-clone
   # equivalent:
   #   git -C /Users/hallandrew/repos/claude-watch fetch origin \
   #     && git -C /Users/hallandrew/repos/claude-watch merge --ff-only origin/main
   ```

   Do this even for a compiled-daemon-only change: the daemon IS baked (step b handles it), but the Python CLI surface is bind-mounted, so a current main clone is the only way bind-mounted fixes go live.

d. **`make deploy-container`** — the single force-recreate. Run it FROM the durable build worktree so the freshly-built image + the correct `COMPOSE_BASE` are used. Fire it via `hostjob` (mirroring step b's build) so the launch survives the host-bash 30s cap cleanly. This KILLS the session:

   ```bash
   hostjob run --label cw-deploy --cwd /Users/hallandrew/repos/.worktrees/claude-watch/main -- make deploy-container
   ```

   NUANCE vs step b's `hostjob wait cw-build`: `make deploy-container` KILLS this session mid-run (the recreate tears down the issuing container), so — unlike the build — you will NOT get to `hostjob wait cw-deploy` it; the recreate ends this session first. Firing it via `hostjob` is exactly what lets the HOST daemon carry the recreate to completion even though the issuing session dies — the fresh session then resumes automatically via `CLAUDE_AUTO_CONTINUE`. (If you already ran `make container-build` in step b and want to skip the re-build the `deploy-container` dependency triggers, that's fine — an unchanged image rebuild is a fast no-op given BuildKit layer caching.) `make deploy-container` is still a SINGLE atomic host-daemon operation — no `&`, no `nohup`, no split; `hostjob` only detaches the launch so it outlives the 30s cap and this session's teardown.

e. **Post-recreate validation** (in the FRESH session): confirm the container is freshly `Up` and the intended change is live. Good defaults:

   ```bash
   # container is freshly recreated (short uptime, old container gone):
   docker ps --filter name=claude-container --format "{{.Names}}\t{{.Status}}"
   # the intended change is live — e.g. a new baked skill / obligation / env var:
   ls /opt/claude-container/skills/          # freshly-baked skills present
   obligations list --json                   # gates re-seeded
   ```

   A common self-redeploy check: drop a marker before the recreate (`date -u +%s > ~/.cache/claude-watch/redeploy-marker`) and read it back in the fresh session alongside a fresh container uptime — a readable marker + fresh uptime + an active session = redeploy validated.

## COMPOSE_FILE discovery gotcha (load-bearing)

`make deploy-container` handles this for you (it sets `COMPOSE_FILE` to base + config-dir override), but if you ever invoke `docker compose … --force-recreate` DIRECTLY, a BARE invocation from the wrong cwd fails with **"no configuration file provided: not found"** — docker only auto-discovers a compose file literally named `docker-compose.yml` in the current dir, and the gitignored override never lives in a worktree. So a direct recreate from a worktree merges ZERO override and recreates with NONE of the operator's personal bind-mounts (the recurring "clipboard / cron mount missing after recreate" bug). Point `COMPOSE_FILE` at the base + config-dir override yourself (mirrors what the Makefile does) via `host-bash` `run_script` (the `:`-joined `COMPOSE_FILE` and the conditional belong in `run_script`, not `run_command`):

```bash
base=/Users/hallandrew/repos/claude-watch/examples/compose/docker-compose.yml
override=$HOME/.config/claude-container/docker-compose.override.yml
if [ -f "$override" ]; then
  COMPOSE_FILE="$base:$override" docker compose up -d --force-recreate claude-container
else
  COMPOSE_FILE="$base" docker compose up -d --force-recreate claude-container
fi
```

Prefer `make deploy-container` (which encodes exactly this plus the `--env-file` and prepare-host-claude-state steps) over a hand-rolled `docker compose` invocation.

## Important

- **NEVER redeploy via `/exit`, `cw --clear`, `self-clear`, or any session-context-clearing op.** Those clear/kill the SESSION — they do NOT recreate the container from the image, so they pick up NONE of an image / env-var / mount change; and an `/exit`-driven recreate (the claude-code auto-upgrader can fire `/exit`) can boot-loop on `~/.local/bin/claude: not found` before the versions volume is populated. The ONLY correct redeploy is `make deploy-container` via `host-bash` (the host daemon owns the recreate).
- Issue every step via `host-bash` — the in-container shell has no `docker`. A raw `make deploy-container` in the in-container `Bash` tool fails `docker: not found`.
- `make deploy-container` is a SINGLE atomic host-daemon operation (safe for self-redeploy) — never add `&` / `nohup` / `disown`, never split into `down && up`.
- Restart is NOT recreate: `/claude-container:restart-container` reuses the SAME container (same image / env / mounts) and only re-runs the process tree. This skill recreates FROM the image and is the only one that picks up an image / env-var / mount change.
- `host-bash` `run_command` splits on shell operators even inside quotes, so the `COMPOSE_FILE="$base:$override"` form and the multi-step sequences here belong in `run_script`, not `run_command`.
- claude-watch redeploy = BOTH surfaces: the build worktree + `make container-build` cover the BAKED surface (daemon, skills, hooks); the main-clone sync covers the BIND-MOUNTED surface (`obligations` / `session-task` Python CLIs). A redeploy that skips the main-clone sync leaves merged bind-mounted fixes DORMANT.
