# container/

This directory is part of claude-watch — see the [top-level README](../README.md) for context.

Containerized deployment of Claude Code + `claude-watch` + tmux as a single Docker image. Goal: a portable Claude Code environment that runs the same way on Linux servers and macOS work laptops, and that can replace a native install on a host as the default deployment mode.

## Quick start

The host-side launcher is [`bin/claude-tmux`](bin/claude-tmux). Install it once with:

```
ln -sf "$(pwd)/bin/claude-tmux" ~/bin/claude-tmux
```

(Run from this directory, or substitute the absolute path.)

Then from any directory:

```
claude-tmux               # launch the tmux session via the image ENTRYPOINT
claude-tmux bash          # debug shell inside the container
claude-tmux --help        # usage + mount/env surface
```

The wrapper auto-detects docker access: it tries bare `docker` first, then `sudo docker`. If your invoking user is not in the `docker` group, `sudo docker` with NOPASSWD is the supported fallback path; if `sudo -n docker ps` stops working the wrapper will error with a fix-it pointer.

Override the image tag via `CLAUDE_CONTAINER_IMAGE` (default: `claude-container:dev`).

## Build

```
docker build -t claude-container:dev -f container/Dockerfile container/
```

Or `cd container && docker build -t claude-container:dev .` if you prefer.

The Dockerfile is a multi-stage build:

- **Stage 1 (`claude-watch-builder`)**: `rust:1-bookworm` clones
  [`hndrewaall/claude-watch`](https://github.com/hndrewaall/claude-watch)
  at a pinned commit (build-arg `CLAUDE_WATCH_REF`) and runs
  `cargo build --release`. Output: `/build/target/release/claude-watch`,
  stripped + LTO-optimised via the upstream `Cargo.toml`'s
  `[profile.release]`.
- **Stage 2 (final)**: `debian:bookworm-slim` copies just
  `/usr/local/bin/claude-watch` from stage 1. Builder and runtime share the
  bookworm libc, so the binary runs without glibc-mismatch surprises.

To pin to a different upstream revision:

```
docker build --build-arg CLAUDE_WATCH_REF=<sha-or-tag> -t claude-container:dev container/
```

The `entrypoint.sh` creates a single-pane tmux session by default (claude
only). Set `CLAUDE_CONTAINER_SIDEBAR=1` to enable the optional 2-pane
layout with `claude-watch` in a 25%-wide right sidebar. See the "Pane
layout" section below for both shapes.

## Run (without the wrapper)

```
sudo -E docker compose -f container/compose.yml run --rm claude-container
```

Or detached for `docker exec` smoke tests:

```
sudo -E docker compose -f container/compose.yml up -d
```

**The `-E` flag is required**. `compose.yml` uses `${HOME}` interpolation to
keep the file host-portable, but `sudo` strips the caller's environment by
default — bare `sudo docker compose up` expands `${HOME}` to `/root` (sudo's
environment), silently pointing the bind mounts at `/root/.claude` /
`/root/repos` and breaking JSONL discovery + auth chain. `sudo -E` preserves
the invoking user's environment, including `HOME`. Equivalent:
`sudo HOME=$HOME docker compose up -d` if you want to forward only `HOME` and
nothing else. The `claude-tmux` wrapper handles this transparently by passing
`-v "$HOME/.claude:..."` to `docker run` (no compose interpolation in the
sudo'd shell), which is why the wrapper is the recommended entrypoint.

Or for one-off debugging without the tmux entrypoint:

```
docker run --rm -it claude-container:dev bash
```

## Pane layout

### Default (one full-screen pane)

The entrypoint creates a tmux session named `claude-container` with a single
full-screen pane:

- **Pane 0 (`claude-container:0.0`)** — `claude` running interactively.
  This is the pane the user types into.

This matches the `dashboard` script's documented default in
[`docs/dashboard-layout.md`](../docs/dashboard-layout.md): "no config file
= claude-only single full-screen pane". A 2-pane layout with a 25%-wide
sidebar renders as a too-narrow strip in typical browser terminals (the
ttyd web console at `examples/compose/`), so the in-container daemon is
opt-in.

### Sidebar mode (`CLAUDE_CONTAINER_SIDEBAR=1`)

Setting `CLAUDE_CONTAINER_SIDEBAR=1` in the container's environment
restores the previous 2-pane layout:

- **Pane 0 (`claude-container:0.0`, left, ~75%)** — `claude` running
  interactively.
- **Pane 1 (`claude-container:0.1`, right, ~25%)** — the in-container
  `claude-watch` daemon (bare `claude-watch` invocation). Reads pane 0 via
  in-container `tmux capture-pane`, enforces token-stall / heartbeat /
  context-warning checks against the in-container claude.

The daemon is still available outside sidebar mode — exec into the
container and run `claude-watch` yourself, or inspect `tmux capture-pane`
output directly.

The daemon picks up its config from `/etc/claude-watch/config.toml` (baked
into the image, sourced from `claude-watch.config.toml` in this directory).
Container-specific deltas from a typical host config:

- `[tmux] dashboard_pane = "claude-container:0.0"` / `dashboard_session =
  "claude-container"` — pinned to the in-container tmux session, not a host
  `dashboard` session.
- Logs land at `/tmp/claude-watch.jsonl` (uid 1000 writable, ephemeral).
- `watcher_monitor`, `auto_update`, `reauth`, `task_watch`, `hybrid`
  disabled — those depend on host integrations.

To inspect pane 1 from another shell on the host (only meaningful when
`CLAUDE_CONTAINER_SIDEBAR=1` was set at container start):

```
sudo docker exec -it <container> tmux attach -t claude-container
sudo docker exec <container> tmux capture-pane -t claude-container:0.1 -p
sudo docker exec <container> cat /tmp/claude-watch.jsonl
```

In sidebar mode, if the daemon fails to start (config parse error, etc.)
pane 1 drops to a bash prompt with the exit code printed, so the failure
is visible on `tmux attach` instead of disappearing into a closed pane.

## In-container user name

The container runs as a user literally named `hndrewaall` (uid 1000, gid 1000).
This is an *in-container* identity only — the HOST user can have any name; the
bind mounts use `${HOME}` on the left side so the right (container) side is the
only place the username matters. The image hardcodes uid 1000 so bind-mounted
files round-trip without root-owned artifacts on hosts where the invoking user
is also uid 1000 (the typical case).

If your host user is not uid 1000, override at build time (extend the Dockerfile
to `useradd --uid $HOSTUID`) or rebuild with matching uid/gid; otherwise
bind-mounted writes will produce files owned by uid 1000 from the host's
perspective.

## Blast radius

The `claude-tmux` wrapper passes EXACTLY the following surface into the container — nothing else from the host is visible.

**Bind mounts** (host -> container, read-write, all uid 1000):
- `~/.claude` -> `/home/hndrewaall/.claude` — session JSONL, credentials, project state
- `~/repos` -> `/home/hndrewaall/repos` — code (all git repos)
- `$PWD` -> `/workspace` — the directory `claude-tmux` was invoked from

**Named volumes** (managed by docker, not bind-mounted from the host):
- `claude-container-versions` -> `/home/hndrewaall/.local/share/claude` — persists the in-container claude binary's auto-updated `versions/<ver>/` directories across `--rm` container exits. Without this, every container restart resets to the image-baked claude version. See "Volume management" below.

**Env vars passed in** (only forwarded if set on the host; everything else is filtered):
- `CLAUDE_CODE_SSE_PORT` — VSCode IDE integration port (HTTP/SSE on host loopback, load-bearing)
- `CLAUDE_CODE_IDE_HOST_OVERRIDE` — host the claude binary dials for SSE; defaults to `host.docker.internal` if unset (the wrapper supplies the default)
- `ANTHROPIC_API_KEY` — only if set on the host
- `CLAUDE_*` / `ANTHROPIC_*` — any other vars matching these prefixes are forwarded automatically
- `NODE_EXTRA_CA_CERTS` / `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE` / `CURL_CA_BUNDLE` — corporate-CA bundles. The wrapper forwards the env var AND auto bind-mounts the CA file (read-only) at the SAME path inside the container so the env-var reference resolves. Needed when the host is VPNed into a corp network with TLS MITM (Salesforce et al.); without this the in-container `claude` binary fails with "Unable to connect to API: SSL certificate verification failed."
- `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` (and lowercase variants) — forwarded verbatim. The proxy hostname needs to be reachable from the container's netns; `host.docker.internal` is wired via `--add-host`, so a host-loopback corp proxy works without extra config.
- `CLAUDE_HOST_HOOKS_DIR` — host directory containing settings.json hook scripts. When `~/.claude/settings.json` (bind-mounted in) references a hook by ABSOLUTE host path (e.g. corp telemetry hooks installed under `~/.devbar/bin/`), set this to the host dir; the wrapper bind-mounts it read-only at the SAME path inside the container so the hook resolves. Without this, Claude Code prints `SessionStart:startup hook error — /bin/sh: 1: <path>: not found` and the hook silently fails.

**Fail-loud guards**: if any of `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`, or `CLAUDE_HOST_HOOKS_DIR` is set on the host but points at a missing path, the wrapper emits a stderr `WARNING` line before invoking docker so the silent-fallback-to-system-CA case is visible. Pass `--suppress-warnings` to silence (also used by the test suite). Run `claude-tmux verify` against a live container to assert that every forwarded env var is visible AND every mount path resolves inside.

**Cross-arch hook shim**: if `~/.claude/settings.json` references a hook binary built for a different architecture (e.g. a Mac telemetry hook bind-mounted into this Linux container), Linux's `exec()` bounces it with "Exec format error" and Claude Code logs the failure on every hook event. Wrap such commands in `exec-hook <path>` — the shim is baked into the image at `/usr/local/bin/exec-hook`, inspects magic bytes, exec's ELF / shebang targets transparently, and silently no-ops on Mach-O / unknown formats with a single stderr heads-up per target per container lifetime. See `container/hooks-shim/exec-hook` for the full magic-byte table and the future-scope IPC-forwarder TODO.

**Auto-rewrite for the whole settings.json (opt-in)**: when manually wrapping every hook command in the host `settings.json` would mutate the host install, set `CLAUDE_CONTAINER_REWRITE_HOOKS=1` in the container env. The entrypoint runs `generate-hooks-shim-settings` to produce a container-local copy of `~/.claude/settings.json` with every hook command wrapped in `/usr/local/bin/exec-hook`, then launches claude with `--setting-sources project,local --settings /tmp/claude-shim/settings.json`. The `--setting-sources project,local` flag filters the bind-mounted host `~/.claude/settings.json` (the "user" tier) out of Claude Code's settings cascade, and `--settings` loads the rewritten shim file as the user-tier replacement. The shim's `env` / `permissions` / other top-level keys are passed through unchanged by the rewriter, so the operator's host config still applies — just with the cross-arch hooks safely wrapped. Without the `--setting-sources` filter, Claude Code's settings merge would be ADDITIVE (load both the bare host hooks AND the wrapped shim hooks) and the bare ones would STILL hit "Exec format error" on every hook event (PR #143 → v19 workbot validation surfaced this). The host file is never touched on disk. Default off so existing operators see no behaviour change.

**MCP server project-tier write (same opt-in path)**: dropping the user tier from the settings cascade with `--setting-sources project,local` also suppresses MCP server discovery. Claude Code v2.1.141 reads MCP server definitions from `~/.claude.json` (where `claude mcp add ...` writes) via a code path that's gated on the `user` tier being in `--setting-sources` — it does NOT read MCP definitions from any settings.json tier. v21 workbot validation confirmed this: injecting `mcpServers` into the shim settings.json had zero effect on `claude mcp list` / `/mcp` inside the container. To make MCP servers visible without re-enabling the user tier (which would re-introduce the bare-host-hook bug), the entrypoint instead writes a project-tier `.mcp.json` inside `CLAUDE_HOST_PROJECT_DIR` via `generate-project-mcp-json`. Project tier IS in `--setting-sources project,local`, and `.mcp.json` is Claude Code's standard project-level MCP config file. The helper reads `~/.claude.json`'s top-level `mcpServers` block plus any `projects.<host-path>.mcpServers` (top-level wins on collision) and wraps each server's `command` with `/usr/local/bin/exec-hook`: ELF / shebang-script servers (node, python, Linux binaries) run normally; Mach-O / unknown-format servers silently no-op (the server fails to start but no "Exec format error" spam on every invocation). If a Mac-native MCP server is required at runtime, build a Linux-compatible replacement or accept that it won't connect inside the container. The host `~/.claude.json` is never mutated; only `<project-dir>/.mcp.json` is written. If the project dir is a git tree, consider adding `.mcp.json` to `.gitignore` — the file is container-only (paths point at `/usr/local/bin/exec-hook`) and won't be useful outside the container. The write is idempotent (mtime preserved when content matches). Requires `CLAUDE_HOST_PROJECT_DIR` to be set — without it, the helper is a graceful no-op and Claude Code reports "No MCP servers configured" inside the container.

**HTTP bridge for cross-arch MCP binaries (`CLAUDE_MCP_HTTP_BRIDGE`)**: when the operator's `mcpServers` includes a server whose `command` points at a host-only binary that won't run inside the Linux container (typical case: a macOS Mach-O binary like the Salesforce `mcp-adaptor` at `~/.mcp-adaptor/bin/mcp-adaptor-go-<ver>-darwin-arm64`), the default behaviour wraps the command with `exec-hook` → silent no-op on Mach-O → `Failed to reconnect to <name>: ENOENT` in `/mcp`. The shim keeps the container quiet but leaves the server unusable. The escape hatch is to run a tiny HTTP→stdio adapter on the **host** that owns the actual MCP binary and exposes it on a TCP port (e.g. `http://host.docker.internal:8765/mcp`), and then rewrite the in-container `.mcp.json` entry from stdio shape to Claude Code's native HTTP MCP transport so the in-container `claude` dials the adapter instead of trying to exec a cross-arch binary. Setting `CLAUDE_MCP_HTTP_BRIDGE` to a colon-separated list of `name=url` pairs flips that for each named server: `generate-project-mcp-json` writes `{"type": "http", "url": "<url>"}` for those entries (dropping the stdio-specific `command` / `args` / `env` / `transport` fields) and leaves the other servers in their default stdio + exec-hook shape. The match is by exact MCP server name (NOT command path or glob), and unset / empty means no rewriting (default, backward-compatible). The helper does NOT start the host-side adapter — wire that up out of band (a launchd LaunchAgent on macOS, a manual `mcp-proxy` invocation, or any other stdio→HTTP wrapper such as `mcp-proxy`, `mcphost`, or the upstream `@modelcontextprotocol/sdk` server-side helpers). Example: `CLAUDE_MCP_HTTP_BRIDGE=mcp-adaptor=http://host.docker.internal:8765/mcp` rewrites just the `mcp-adaptor` entry. Multiple pairs: `CLAUDE_MCP_HTTP_BRIDGE=adaptor=http://host.docker.internal:8765/mcp:other=http://host.docker.internal:9000/mcp`. URLs that contain `:` (every URL with a scheme does) parse correctly because the outer split only treats `:<name>=` boundaries as separators — URL scheme/port colons stay attached to their pair. HTTP-specific keys the operator already had on the original entry (`headers`, `alwaysLoad`, `oauth`, `headersHelper`) survive the rewrite verbatim. The host adapter is responsible for whatever env vars the actual MCP binary needs (`GW_PROFILE`, `MCP_ADAPTOR_ENV`, etc.); the env stays on the host side where the binary actually runs.

**Generic "run a bash command on the host" MCP server (`host-bash`)**: a second use of `CLAUDE_MCP_HTTP_BRIDGE` is to give the in-container `claude` a tool that exec's argv on the **host** instead of inside the container. Useful when the container can't reach corp git / host-only CLIs / Mac-side scripts, but the operator's host already can. There's no special container-side support needed for this — the bridge is name-keyed, so a server named `host-bash` in `~/.claude.json` `mcpServers` + a `host-bash=<url>` entry in `CLAUDE_MCP_HTTP_BRIDGE` is enough to surface a second MCP server inside the container. The host-side launcher is bundled in [`examples/compose/bin/mcp-host-bash`](../examples/compose/bin/mcp-host-bash): it runs `uvx mcp-proxy --port 8766 -- uvx cli-mcp-server` (both off-the-shelf, MIT-licensed, no hand-rolled MCP server needed) with an env-var-driven allow-list — `ALLOWED_COMMANDS` (binary whitelist; default `ls,cat,pwd,git,gh,head,tail,grep,find,echo`), `ALLOWED_FLAGS`, `ALLOWED_DIR` (refuse paths outside this tree; default `$HOME`), `COMMAND_TIMEOUT` (per-command wall-clock cap; default 30s), `ALLOW_SHELL_OPERATORS` (block pipes / `&&` / `||` / redirects when `false`; default `false`). Operator overrides via `~/.config/claude-container/mcp-host-bash.env` (sourced before launch). Soft kill switch: `MCP_HOST_BASH_DISABLED=1`. Audit log: `~/.local/state/claude-container/mcp-host-bash.log`. Run `mcp-host-bash --help` for the full surface. **Security caveat**: `run_command` is a substantial privilege escalation. Anything that has API access to the in-container `claude` can now invoke commands on the host's user account. The allow-list defaults are intentionally conservative (no `bash` / `sh`, no shell operators, `$HOME` boundary, 30s timeout); broaden them only as needed for your workflow and audit the log. If the container is compromised or fed a hostile prompt, the blast radius is bounded by the allow-list — keep it tight.

**Selective shim wrapping (`CLAUDE_SHIM_PATTERNS`)**: both rewrite helpers default to wrapping EVERY command they encounter (every hook command + every MCP `command` field) when `CLAUDE_CONTAINER_REWRITE_HOOKS=1`. That's safe (the shim is transparent on ELF / shebang, only Mach-O / unknown formats no-op) and matches the original PR #135 intent. Operators who want to narrow the set of wrapped commands — e.g. wrap only corp telemetry hooks under `~/.devbar/bin/*` and leave everything else untouched — can set `CLAUDE_SHIM_PATTERNS` to a colon-separated list of glob patterns. When the env var is non-empty, the helpers match each command's first whitespace-separated token (the binary path) against the patterns using `fnmatch.fnmatchcase` and only wrap commands where at least one glob matches; non-matching commands pass through verbatim. Unset or empty preserves the existing wrap-everything default, so existing deployments see no behaviour change. Example: `CLAUDE_SHIM_PATTERNS='/Users/*/.devbar/bin/*:/Users/*/.devbar/pkgs/*/bin/*'` wraps only commands whose binaries live under those two corp paths. The same env var feeds both `generate-hooks-shim-settings` (hook commands) and `generate-project-mcp-json` (MCP server commands) — there's no separate hook-only vs MCP-only tuning knob today (file an issue if you need one). Patterns are positive-match only; there's no `!negative` syntax yet. The colon separator parallels `PATH`; paths containing literal `:` are extraordinarily rare on real systems but if you have one, pick a more specific glob that doesn't depend on the colon. Arguments AFTER the first token don't participate in matching, so a pattern like `/Users/*/.devbar/bin/hook` matches `/Users/me/.devbar/bin/hook --flag value | grep foo` even though the rest of the command contains shell syntax.

**Host-project cwd / project-memory key**: Claude Code keys its project memory tree off the cwd at launch (`~/.claude/projects/<urlencoded-cwd>/memory/MEMORY.md`). Default container WORKDIR is `/workspace`, which produces the project key `-workspace` — almost never matching the host's project key (typically `-Users-<you>-repos-<project>`). Set `CLAUDE_HOST_PROJECT_DIR=/absolute/host/path` and the wrapper bind-mounts that path at the SAME absolute path inside the container AND sets it as the container WORKDIR, so the in-container claude's project key matches the host's and project memory loads. Missing-dir = silent fallback to `/workspace` with a stderr WARNING via the existing fail-loud guard.

**Picking the right `CLAUDE_HOST_PROJECT_DIR` for memory loading**: Claude Code's auto-memory tree lives at the project-key derived from the operator's actual host cwd. If you typically launch claude from the workspace root (e.g. `~/repos/`) and only step into individual project subdirs at the slash-command layer, the memory tree lives at `~/.claude/projects/-Users-<you>-repos/memory/MEMORY.md` — NOT at the per-project sub-key. Set `CLAUDE_HOST_PROJECT_DIR` to the workspace root (`/Users/<you>/repos`) rather than a specific project subdir (`/Users/<you>/repos/eichi`) when you want the workspace-level memory loaded. Per-project sub-cwds get their own (typically empty) memory dirs and won't pick up the workspace-level memories. v19 workbot validation made this concrete: `CLAUDE_HOST_PROJECT_DIR=/Users/hallandrew/repos/eichi` resolved the project key correctly (`-Users-hallandrew-repos-eichi`) but found no memory files because Andrew's auto-memory tree lives at the parent (`-Users-hallandrew-repos`).

**Network policy**: default **bridge networking** with an explicit
host-loopback alias. The wrapper invokes
`docker run --add-host=host.docker.internal:host-gateway ...` (no
`--network host`); compose.yml uses `extra_hosts:
["host.docker.internal:host-gateway"]`. The in-container claude binary reads
`CLAUDE_CODE_IDE_HOST_OVERRIDE=host.docker.internal` and dials that name for
its SSE upstream, which the bridge resolves to the host-gateway IP.

`host.docker.internal` is provided natively by Docker Desktop on macOS and
Windows; the `--add-host=host.docker.internal:host-gateway` flag is what makes
it work on Linux too (Docker Engine 20.10+ honors the `host-gateway` magic
value).

**User**: container runs as `hndrewaall` (uid 1000, gid 1000) — matches a
host UID-1000 user so bind-mounted files round-trip without root-owned
artifacts.

**Signal handling**: the wrapper traps `SIGTERM`/`SIGINT` on the host and forwards them via `docker kill --signal=...` to a per-PID container name (`claude-tmux-$$`), so `Ctrl-C` from the host cleanly tears down the in-container tmux session.

## Host-only CLIs

The image bakes the following binaries:

- `claude` — the Claude Code CLI (installed via `npm install -g @anthropic-ai/claude-code`).
- `claude-watch` — the Rust daemon, built from source in the multi-stage `claude-watch-builder` stage and copied into `/usr/local/bin/claude-watch`.
- `audit-hooks` — observability tool that walks `~/.claude/settings.json` and reports per-hook fate (ok-elf / ok-script / silent-no-op / missing / not-wrapped / builtin). See `bin/audit-hooks.py`.
- `cwsr` — in-container self-restart for the `claude` CLI. Runs `npm install -g @anthropic-ai/claude-code@<ver>` then `tmux respawn-pane -k -t claude-container:0.0` so the inner claude rolls in-place WITHOUT requiring the operator to `docker compose restart` the whole container. The wrapping container, MCP bridges, named-volume `~/.local/share/claude/versions/`, and operator's tmux attach all survive — only the inner process rolls. See `bin/cwsr` (`cwsr --help` for usage).
- `trust-workspace` — pre-trusts a workspace path in `~/.claude.json` so Claude Code skips its first-launch trust prompt at that cwd.
- `exec-hook` — magic-byte safe-exec wrapper for hook commands whose target may not be Linux-native. ELF / shebang scripts pass through transparently; cross-arch (Mach-O / PE / unknown) targets silently no-op.

Everything else from the claude-watch source tree — including the Python CLIs under `tools/` (`session-task`, `claude-event`, `obligations`) — is NOT installed into the image. They're discoverable on `PATH` only when the operator bind-mounts `~/repos/claude-watch` into the container at `/home/hndrewaall/repos/claude-watch` (which the [example compose](../examples/compose/) does by default).

The mechanism is a small `/etc/profile.d/claude-tools.sh` fragment baked into the image (see `claude-tools.profile.sh` in this directory). At login / new-shell time it checks for each tool dir under `${HOME}/repos/claude-watch/tools/` and prepends it to `PATH` if present. Missing dirs are silently skipped, so a stripped-down `docker run` with no bind mount still gets a working shell — the bind-mounted CLIs just won't be on `PATH`.

Operational tooling that the operator runs on the **host** (alerting, monitoring, media post-processing, ingest pipelines, etc.) is intentionally NOT installed in the container. The image is meant to be a generic Claude Code + claude-watch sandbox; host-specific tooling stays on the host where it has the right environment, credentials, and filesystem layout. Layer that in via your own image or a sibling bind-mount when you need it.

The [example compose stack](../examples/compose/) takes that "sibling bind-mount" path further by mounting `~/bin` (read-only) alongside `~/repos`, so host-installed CLI symlinks resolve inside the container. Every host-side source path in that compose file is overridable via a `CLAUDE_HOST_*` env var (defaults work for Linux without further config; macOS or corporate-managed-config operators set `CLAUDE_HOST_MANAGED_SETTINGS_DIR` to opt into a host managed-settings dir — note that doing so REPLACES the image-baked `/etc/claude-code/` including the managed-policy CLAUDE.md the image ships). See [examples/compose/README.md](../examples/compose/README.md) "Host state bind-mounts" + "Host paths on non-default layouts (env-var overrides)" for the full table of mounts the example wires up (claude-events, settings dirs, etc.), the per-tier Claude Code settings hierarchy, and the macOS graceful-no-op behavior for paths that don't exist on the host. Host-specific integration mounts (shell-history DBs, messaging attachment dirs, etc.) live in a local `docker-compose.override.yml`, not the public example.

## Baked managed-policy CLAUDE.md

The image ships `container/baked-CLAUDE.md` at `/etc/claude-code/CLAUDE.md` —
the [standard Linux managed-policy location](https://code.claude.com/docs/en/memory#deploy-organization-wide-claude-md)
that Claude Code loads before the user-tier `~/.claude/CLAUDE.md` and the
project-tier `<cwd>/CLAUDE.md`. Managed CLAUDE.md cannot be excluded by
user or project `claudeMdExcludes` settings — that's the contract.

The contents describe the in-container runtime (you're in Linux not on the
host; what's bind-mounted vs not; the cross-arch hook situation; which MCP
bridges are available) so every session starts with a load-bearing
description of the environment, not a vanilla blank slate. The full text
is in [`baked-CLAUDE.md`](baked-CLAUDE.md) in this directory; rebuild the
image to update.

The example compose stack's `CLAUDE_HOST_MANAGED_SETTINGS_DIR` env-var
mount is `/dev/null` by default (graceful no-op) so the baked CLAUDE.md
stays visible. Operators who have a host managed-settings dir set the env
var explicitly — doing so replaces the baked `/etc/claude-code/` wholesale,
which means the host dir must include its own CLAUDE.md if you want the
container-runtime description to remain in context. The simplest path is
to symlink or copy `container/baked-CLAUDE.md` into your host
managed-settings dir alongside whatever else lives there.

## Volume management

The `claude-container-versions` named docker volume holds the in-container claude binary's auto-updated `versions/<ver>/` tree at `/home/hndrewaall/.local/share/claude/`. It is created on first `claude-tmux` invocation (or first `docker compose up`) and persists across `--rm` exits — that's its whole purpose.

Inspect existence + size + driver:

```
docker volume ls --filter name=claude-container-versions
docker volume inspect claude-container-versions
```

Peek inside without launching the full container:

```
docker run --rm -v claude-container-versions:/data alpine ls -la /data
docker run --rm -v claude-container-versions:/data alpine ls -la /data/versions/
```

Nuke (forces fallback to image-baked claude on next launch, then re-populates as the in-container claude auto-updates):

```
docker volume rm claude-container-versions
```

**Drift risk**: the image bakes a known-good claude (`/usr/local/bin/claude`). The named volume captures whatever the in-container claude has self-installed on top. The `~/.local/bin/claude` symlink inside the volume (if present from a prior auto-update) wins on PATH because `~/.local/bin` precedes `/usr/local/bin` — that's expected. If the volume gets torn (partial download, dangling symlink), nuke it; the image-baked floor still works.
