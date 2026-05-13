# Fresh-laptop developer stack

End-to-end `docker compose` example that wires:

- **claude-container** (this repo, under [`container/`](../../container/)) — Claude Code + `claude-watch` + tmux baked into one image.
- **queue-minisite** (this repo, under `queue-minisite/`) — mobile-friendly Flask UI for the `session-task` work queue.
- [**eichi**](https://github.com/hndrewaall/eichi) `search-minisite` — Flask UI for the local sqlite-vec + sentence-transformers semantic-search CLI.
- **ttyd** (this repo, under [`ttyd/`](ttyd/)) — browser-based terminal that attaches to the claude-container's in-container tmux session.

Drop into a freshly cloned setup, run one command, get the integrated experience: a containerised Claude Code shell, a web UI for its work queue at `http://localhost:8000/`, a semantic-search web UI at `http://localhost:8001/`, and a browser terminal at `http://localhost:7681/`.

Looking for periodic event emissions instead? See [`examples/cron/`](../cron/README.md) for a cron-driven `claude-event` example.

## Prerequisites

- Docker Engine (Linux) or Docker Desktop (macOS / Windows). Compose v2 (the `docker compose` subcommand, not legacy `docker-compose`).
- `git`.
- An Anthropic API key (`ANTHROPIC_API_KEY`) if you want the Claude Code service to actually talk to the API.
- A host UID of `1000` is the smoothest path because the container images bake the `hndrewaall` user at uid 1000. Other UIDs work but you'll see permission warnings on bind-mounted state until you adjust the `user:` directives.

## Sibling-repo layout

The compose file uses an in-repo build context (`../../container`) for the
claude-container service and a sibling-repo context (`../../../eichi`) for
eichi search. Clone both repos next to each other:

```sh
mkdir -p ~/code && cd ~/code
git clone https://github.com/hndrewaall/claude-watch.git
git clone https://github.com/hndrewaall/eichi.git
```

Resulting layout:

```
~/code/
  claude-watch/
    container/          <- claude-container source (this repo)
    examples/compose/   <- you run docker compose from here
  eichi/
```

Any parent directory works (`~/code/`, `~/src/`, `/srv/`, etc.) — only the sibling relationship between `claude-watch/` and `eichi/` matters.

## Pre-flight (host side)

The compose stack expects a few host directories + files to exist; the easiest way to seed them is to run each tool natively once before the first `docker compose up`.

1. **claude-watch / session-task state** — exists under `~/.claude/`, `~/.config/session/`, `~/.config/claude/`, `~/claude-events/`. Created automatically by claude-watch / session-task on first use. If you've never run claude-watch on this host, run `tools/session-task/session-task list` from the repo root once — it bootstraps `~/.config/session/queue.json`.
2. **eichi state** — exists under `~/.local/share/eichi/` (index DB) and `~/.cache/huggingface/` (embedding-model cache). `cd ~/code/eichi && uv venv --python 3.11 && uv pip install -e . && eichi index ~/Documents/notes` (or whatever you want to search) gets both. Skip if you only want the queue UI.

## Run

From `claude-watch/examples/compose/`:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
docker compose up
```

The first build will take a while (Rust + sentence-transformers wheels + Node 20 + the multi-stage claude-watch builder). Subsequent runs are cached.

Once up:

- **http://localhost:8000/** — queue-minisite UI. Lists pending / running / blocked queue items, exposes Stop / Abandon / Force-start buttons.
- **http://localhost:8001/** — eichi search UI. Type a natural-language query, get top-K hits across whatever you've indexed.
- **http://localhost:7681/** — ttyd browser terminal. Drops you directly into the claude-container's tmux session (same view as `docker compose exec claude-container tmux attach`).

## Host state bind-mounts

The `claude-container` service bind-mounts a curated subset of host state into
the container so an in-container `claude` / `claude-watch` boots with the
operator's real config instead of a vanilla blank slate. The mount set is
deliberately narrow — only the paths that materially affect Claude Code
behavior or claude-watch monitoring.

| Host path (default) | env-var override | Container path | Mode | Why |
|---|---|---|---|---|
| `/etc/claude-code` | `CLAUDE_HOST_MANAGED_SETTINGS_DIR` | `/etc/claude-code` | `ro` | Managed / enterprise Claude Code settings tier (`managed-settings.json`, etc.). macOS default is `/Library/Application Support/ClaudeCode` — override. |
| `~/.claude` | `CLAUDE_HOST_CONFIG_DIR` | `/home/hndrewaall/.claude` | `rw` | User-global Claude Code state: session JSONLs, project state, cache, agent definitions (`agents/*.md`). Claude writes here continuously. |
| `~/.claude.json` | `CLAUDE_HOST_CONFIG_FILE` | `/home/hndrewaall/.claude.json` | `rw` | User-global top-level Claude Code config (MCP servers, model prefs, project allow-list). |
| `~/repos` | `CLAUDE_HOST_REPOS_DIR` | `/home/hndrewaall/repos` | `ro` | Host repo trees (also carries project-tier Claude Code config in each repo's `.claude/`). Read-only — confine container edits to `/workspace` or explicit clones. |
| `~/bin` | `CLAUDE_HOST_BIN_DIR` | `/home/hndrewaall/bin` | `ro` | Launcher / shim scripts (mostly symlinks into `~/repos/*/bin`). Resolves correctly because `~/repos` is also mounted. |
| `~/claude-events` | `CLAUDE_HOST_EVENTS_DIR` | `/home/hndrewaall/claude-events` | `rw` | claude-event JSONL spool. Host producers write, in-container `claude-event-watch` consumes. |
| `~/.config/session` | _(not overridable; shared with queue-minisite)_ | `/home/hndrewaall/.config/session` | `rw` | session-task queue.json (same path the queue-minisite mounts). |

Host-specific integration mounts (shell-history databases, messaging
attachment dirs, etc.) are intentionally out of scope for this example.
Add them in a local `docker-compose.override.yml` if your operator setup
needs them.

### Host paths on non-default layouts (env-var overrides)

Every Phase-2 mount source is overridable via a `CLAUDE_HOST_*` env var
(see the "env-var override" column in the table above). Defaults resolve
to the standard Linux locations under `${HOME}` (or `/etc` for the
managed-settings tier). Override via `.env` in this directory — `docker
compose up` auto-loads it. A starting `.env.example` ships in this
directory; copy to `.env` and uncomment the lines you need.

This is the recommended fix for any host whose Claude Code config or
operator-tooling paths live somewhere other than the defaults. The
most common case is **macOS**, where Claude Code's _managed_ settings
tier lives at a different path than on Linux:

| Tier | Linux default | macOS default |
|---|---|---|
| Managed / enterprise (`managed-settings.json`) | `/etc/claude-code/` | `/Library/Application Support/ClaudeCode/` |
| User-global (`~/.claude/`) | `${HOME}/.claude` | `${HOME}/.claude` (same) |
| User-global top-level (`~/.claude.json`) | `${HOME}/.claude.json` | `${HOME}/.claude.json` (same) |
| Project-level (`.claude/` in a repo) | `${HOME}/repos/*/.claude` | `${HOME}/repos/*/.claude` (same; arrives via the repos mount) |

The user-tier paths are the same on both OSes per the upstream
[Claude Code settings docs](https://code.claude.com/docs/en/settings) —
no override needed unless your host intentionally relocates them. The
**managed-settings tier** is the one that does differ between Linux and
macOS, and is the env var most macOS operators will want to set.

A minimal macOS `.env`:

```ini
# Point at the macOS managed-settings location instead of /etc/claude-code.
CLAUDE_HOST_MANAGED_SETTINGS_DIR=/Library/Application Support/ClaudeCode
```

`.env` values support whitespace literally — no quoting needed for the
embedded space in `Application Support`. If a particular host-state
path doesn't exist on your machine, leave it unset — see "macOS
graceful no-op" below for the bind-mount behavior on missing source
paths.

### macOS graceful no-op

The compose file uses `${HOME}` interpolation on the host side, so the source
paths resolve to `/Users/<you>/...` on macOS. Most of the host-state paths
above (`~/claude-events`, `~/bin`) don't exist on a fresh macOS
workbot. Docker Desktop's bind-mount semantics
auto-create empty directories at the source location when a mount references
a missing path, so the container sees empty dirs — functionally equivalent to
"no host state at all". The in-container claude-watch and claude tolerate
empty/missing state gracefully (they create what they need on first use).

If you're on macOS and want a specific host-state path to actually contain
something, pre-create it on the host before the first `docker compose up`
(e.g. `mkdir -p ~/claude-events`). Otherwise, expect empty mounts and
"vanilla state" behavior for the missing surfaces — which is the same
experience you had before this PR.

(`~/.claude.json` is a file, not a directory; if it's missing on the host
Docker Desktop auto-creates it as an empty directory, which the in-container
claude then ignores. That's the intentional no-op path on a fresh macOS
host.)

## Use the Claude Code shell

The `claude-container` service runs in the foreground by default. To drop into the in-container tmux session:

```sh
docker compose exec claude-container bash
# inside the container:
claude
```

Or use the standalone `claude-tmux` wrapper at [`container/bin/claude-tmux`](../../container/bin/claude-tmux) — it's a more ergonomic entrypoint than `docker compose exec` for interactive use. See [`container/README.md`](../../container/README.md) for details.

### First-launch trust prompt

Claude Code normally shows a "Quick safety check: Is this a project you created or one you trust?" prompt the first time it runs in a new cwd. The `claude-container` entrypoint pre-seeds the trust state for `/workspace` (the Dockerfile `WORKDIR` and the in-container tmux pane's cwd) before launching tmux, so the prompt is skipped on every boot — you land directly at the Claude Code idle prompt.

The pre-seed writes `projects["/workspace"].hasTrustDialogAccepted = true` into the bind-mounted `~/.claude.json`, preserves every other project entry already in the file, and is idempotent (re-running on every container boot is a no-op after the first). When the bind-mount is missing or read-only the entrypoint logs a warning and falls back to showing the prompt — same UX as a stock upstream image.

To pre-trust a different cwd in a downstream image, set `WORKSPACE=/custom/path` in the container env; the entrypoint passes it through to `trust-workspace`. To pre-trust additional paths inside an already-running container, run `docker compose exec claude-container trust-workspace /another/path`.

## ttyd web console

After `docker compose up -d`, a browser-attachable terminal is available at
**http://localhost:7681/** that drops you directly into the claude-container's
tmux session — the same view as `docker compose exec claude-container tmux
attach`. Useful when you want a Claude Code shell from a tablet / phone / second
machine on the LAN without setting up SSH.

### How it's wired

Both `claude-container` and `ttyd` share the named volume `claude-tmux-socket`
mounted at `/tmp/tmux-1000` — tmux's default socket directory for uid 1000.
The claude-container entrypoint creates the session on the default socket
(`tmux new-session -d -s claude-container ...`); ttyd then attaches via
`tmux -S /tmp/tmux-1000/default attach-session -t claude-container`. Both
services run as uid 1000 so socket permissions align.

### Cross-platform volume perms

On Docker Desktop (macOS / Windows), the bundled `tmux-socket-init` service
chowns the shared `/tmp/tmux-1000` volume to uid 1000 mode 0700 on first
start. This is necessary because Docker Desktop's volume layer does not
propagate the image-side directory perms the way native Linux Docker does,
so the named volume otherwise comes up root-owned and tmux refuses to
bind its socket. Harmless no-op on Linux. `claude-container` and `ttyd`
both `depends_on` the init service with `condition:
service_completed_successfully`, so cold-start ordering is guaranteed.

### Theme + font

The default xterm.js theme is Solarized-dark (Ethan Schoonover's public-domain
palette). The default font is `Menlo` with fallback to `monospace`. Override
either by editing the `-t theme=…` / `-t fontFamily=…` flags in the
`ttyd.command` list in `docker-compose.yml`, or by forking
`ttyd/Dockerfile` to bundle a custom font into the image.

### Autodark (page chrome matches system color-scheme)

The ttyd image bundles a build-time-patched `index.html` that adds a
`prefers-color-scheme` media-query CSS block so the page background
around the xterm.js terminal flips between Solarized base03 (dark) and
base3 (light) to match the operator's system color-scheme. Without this,
macOS Safari / Chrome in dark mode would render a white frame around the
dark terminal.

A small `<script>` block in the same patch flips the xterm.js runtime
theme to match the system color-scheme on page load, and reapplies it on
a 2-second `setInterval` to defend against ttyd's post-connect
`SET_PREFERENCES` WebSocket message that otherwise overwrites the
client-side theme. Full details in
[`ttyd/inject-autodark.py`](ttyd/inject-autodark.py).

If you don't want the autodark behavior, drop the `-I /usr/local/share/ttyd/index.html`
line from `ttyd.command` in `docker-compose.yml`. ttyd will then serve
its upstream bundled HTML unchanged.

### Security note

The published port `7681` is unauthenticated by design for local-only
development. Do **NOT** expose this port publicly without adding an
authentication layer in front (oauth2-proxy, nginx basic-auth, Cloudflare
Access, etc.). ttyd's `--writable` flag means anyone reaching the port has
full shell access to the in-container tmux session — which, by extension,
has read access to the bind-mounted `~/.claude` and `~/repos`.

## First-run indexing (eichi)

The `eichi-search` container will start fine with an empty index but return zero results. Populate it from the host:

```sh
cd ~/code/eichi
uv venv --python 3.11
uv pip install -e .
eichi index ~/Documents/notes        # any directory you want searchable
eichi index ~/.claude/projects       # agent transcripts (a useful corpus)
```

The container will see the updated index next request — no restart required (sqlite-vec re-reads on connect).

## Caveats

### Container username is hardcoded

In-container paths (right-hand side of `volumes:`) hardcode `/home/hndrewaall/...` because the `hndrewaall` user is baked into the `claude-container` and `eichi-search` Dockerfiles at uid 1000. Your **host** user can be anything — bind-mount left-hand sides use `${HOME}` interpolation. If your host UID is not 1000, the bind-mounted state directories will look root-owned to the container; the cleanest fix is to add `user: "$(id -u):$(id -g)"` to each service and `chown` the host directories before launching.

### macOS — host UID 501 vs container UID 1000

macOS user accounts default to **uid 501**, not 1000 — and the container images bake the in-container user at uid/gid `1000:1000` (with `queue-minisite` / `eichi-search` pinning `user: "1000:1000"` in this compose file). The two don't match, and yet the stack works on macOS without any manual fixup. Why:

- **Docker Desktop (macOS / Windows)** runs the engine inside a hidden VM and routes bind mounts through a userland file-sharing layer (gRPC-FUSE / VirtioFS). That layer transparently remaps file ownership so the container sees its expected uid (1000) regardless of the host file's actual owner on the Mac filesystem. Reads + writes round-trip without permission errors. This is purely a Docker Desktop convenience and does NOT apply to Linux.
  - **Caveat for Mac users editing container-written files natively:** the remapping is per-mount, not bidirectional metadata sync. Files the container creates under a bind-mounted path are recorded by the VM as `1000:1000`, and `stat` on the macOS side (uid 501, gid 20 by default) shows them owned by an unknown uid. Reads typically still work via Docker Desktop's permissive defaults, but native editors that check ownership before writing (or any tooling that does `chown` / `chmod`) can complain. Fixes, in order of decreasing effort: (a) `sudo chown -R 501:20 <path>` after the container finishes, (b) add `user: "$(id -u):$(id -g)"` overrides per the Linux instructions below so the container writes as uid 501 directly, or (c) keep editing those files inside the container (via `docker compose exec` or the ttyd console) and treat the host copy as read-only.
- **Linux dev boxes** (native Docker Engine, no Docker Desktop) run the engine directly against the host kernel — bind mounts pass through unchanged, so a uid-1000 container process writing to a host directory owned by uid 1500 will produce files literally owned by uid 1000 on the host, and reading host-owned files may EACCES depending on mode bits.

#### macOS — Docker Desktop file-sharing allowlist

Docker Desktop on macOS ships with a default file-sharing allowlist (`/Users`, `/tmp`, `/private`, `/var/folders`). Bind-mount source paths outside that list are refused at container-start with a "path not shared from the host" error. The user-tier paths (`~/.claude`, `~/repos`, etc.) live under `/Users/<you>` and just work, but if you override `CLAUDE_HOST_MANAGED_SETTINGS_DIR` to the macOS managed-settings location (`/Library/Application Support/ClaudeCode`), that path is outside the default share list and needs to be explicitly added via Docker Desktop → Settings → Resources → File Sharing → "+" → pick the directory → Apply & Restart. The same applies to any custom `CLAUDE_HOST_*` override pointing outside `/Users`. Paraphrased rule: paths outside Docker Desktop's default share list need to be explicitly added via Docker Desktop → Resources → File Sharing.

If you're on a Linux box with a non-1000 host UID, you have three options, in order of decreasing effort:

1. **Run as a uid-1000 user.** Easiest if you're setting up a dedicated dev account anyway — `useradd -u 1000 ...` (or repurpose the existing user) and everything just works.
2. **Override `user:` on each service.** Replace the `user: "1000:1000"` lines in `docker-compose.yml` with `user: "$(id -u):$(id -g)"` (or expand the literal numbers) and `chown -R` the bind-mounted host directories to that same uid/gid before launching. The container's named user (`hndrewaall`) is still uid 1000 inside the image; the override only changes which uid the process runs as.
3. **Rebuild the images with matching uid/gid.** Pass `--build-arg HOSTUID=$(id -u) --build-arg HOSTGID=$(id -g)` through and extend the Dockerfile's `useradd` line. Heaviest path, only worth it for long-lived deployments.

The `claude-container` service does NOT pin a `user:` directive — it inherits from the Dockerfile's `USER hndrewaall` (uid 1000) directly. On Linux with a non-1000 host UID, bind-mounted `~/.claude` / `~/repos` will look root-owned to the container; same options apply.

### Skipping services

`claude-container`, `queue-minisite`, and `eichi-search` are independent — comment any one out in `docker-compose.yml` and the rest still work. For example, `docker compose up queue-minisite eichi-search` skips the heavy Rust + Node build of the claude-container image. The `ttyd` service has a hard dependency on `claude-container` (nothing to attach to otherwise); skip both together or neither.

### No upstream auth gate

`queue-minisite` and `eichi-search` are designed to sit BEHIND an authentication proxy (oauth2-proxy, nginx `auth_request`, etc.). The included compose binds them directly to `localhost:8000` / `localhost:8001` with no gate — fine for local single-user dev, NOT fine for exposure on a public IP. Don't `-p 0.0.0.0:8000:8000` this without an auth layer in front.

## Tear down

```sh
docker compose down              # stop + remove containers (volumes survive)
docker compose down -v           # also nuke the claude-container-versions volume
```

The bind-mounted host state under `~/.claude`, `~/.config/session`, `~/.local/share/eichi`, etc. is untouched by `down` — only named volumes go.
