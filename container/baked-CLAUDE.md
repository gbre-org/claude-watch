# claude-container — runtime environment

This file is the **managed-policy CLAUDE.md** baked into the
[claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container)
image at `/etc/claude-code/CLAUDE.md`. Claude Code loads it at session start,
before any user-level (`~/.claude/CLAUDE.md`) or project-level
(`<cwd>/CLAUDE.md`) instructions. It exists so every session inside the
container starts with a load-bearing description of the runtime — what's
real, what's a bind-mount, what doesn't work — without depending on host
config the operator may or may not have wired up.

It is **container-owned, not user-owned**: do not edit
`/etc/claude-code/CLAUDE.md` from a session. The source of truth lives at
`container/baked-CLAUDE.md` in the claude-watch repo; rebuild the image to
pick up changes.

---

## You are running inside a Linux container

If you are reading this file via the standard CLAUDE.md load path, you are
**inside the `claude-container` Docker image**, not on the operator's host
machine. The distinction matters for many decisions:

- `uname -a` returns `Linux <hostname> ... GNU/Linux` regardless of what
  the host OS is. A macOS host bind-mounts files into this Linux userland.
- Binaries built for the host architecture (typically macOS Mach-O / arm64
  on developer laptops) **cannot execute inside this container**. Linux
  rejects them with "Exec format error". See "Cross-arch binaries" below
  for the shim that handles this gracefully.
- The container user is `hndrewaall` (uid 1000, gid 1000). This is an
  in-container identity, hardcoded in the Dockerfile to match a typical
  uid-1000 host user so bind-mounted files round-trip without root-owned
  artifacts. The host user can have any name.
- Hostname is typically `claude-container-<rand>` or whatever
  `docker run --name` was passed; do not infer the host identity from it.

**Quick self-check**: if you need to confirm "am I in the container?",
run `cat /etc/claude-code/CLAUDE.md | head -3`. If you see this file's
header, you are in the container. The host has no `/etc/claude-code/`
unless the operator explicitly created one.

## What is bind-mounted from the host

The
[example compose stack](https://github.com/hndrewaall/claude-watch/blob/main/examples/compose/docker-compose.yml)
documents the standard mount surface. Defaults (operator can override
each via `CLAUDE_HOST_*` env vars):

| In-container path | Host source | Mode | Purpose |
| --- | --- | --- | --- |
| `/home/hndrewaall/.claude/` | `${HOME}/.claude/` | rw | session JSONL, project state, settings, agents/, hooks-referenced files |
| `/home/hndrewaall/.claude.json` | `${HOME}/.claude.json` | rw | top-level Claude Code config (MCP server registry, project allow-lists) |
| `/home/hndrewaall/repos/` | `${HOME}/repos/` | ro | host repo trees (read-only so the container can't scribble on working trees) |
| `/home/hndrewaall/bin/` | `${HOME}/bin/` | ro | operator-curated launcher scripts |
| `/etc/claude-code/` | host managed-settings dir if `CLAUDE_HOST_MANAGED_SETTINGS_DIR` set | ro | host MDM / enterprise policy |
| `${CLAUDE_HOST_PROJECT_DIR}` | same path on host | rw | project cwd (so the project-memory key matches the host's) |
| `${CLAUDE_HOST_HOOKS_DIR}` | same path on host | ro | corp telemetry hook scripts referenced by `~/.claude/settings.json` |

`${HOME}/repos` is **read-only**. Do not try to `git commit` from inside
the container against a path under `/home/hndrewaall/repos/`. Use
`${CLAUDE_HOST_PROJECT_DIR}` (rw) for development work, or `git push`
from the host.

## CLAUDE.md load order inside the container

Claude Code walks several locations at session start. In the container,
the cascade resolves like this (broadest first, narrowest last; later
files take precedence on adherence but all are concatenated into
context):

1. **Managed policy** — `/etc/claude-code/CLAUDE.md` (this file).
2. **User** — `~/.claude/CLAUDE.md` (bind-mounted from the host's
   `${HOME}/.claude/CLAUDE.md`, if present).
3. **Project** — `<cwd>/CLAUDE.md` or `<cwd>/.claude/CLAUDE.md`
   (whichever the operator's `CLAUDE_HOST_PROJECT_DIR` points at).
4. **Local** — `<cwd>/CLAUDE.local.md` (gitignored by convention).

This file (the managed-policy one) **cannot be excluded** by user or
project settings — that's by design and matches the
[Claude Code managed-CLAUDE.md contract](https://code.claude.com/docs/en/memory#deploy-organization-wide-claude-md).

## MCP servers

MCP server definitions live in `~/.claude.json` `mcpServers` on the host,
which is bind-mounted in. Claude Code's MCP discovery path is gated on
the `user` settings tier being in `--setting-sources`. When
`CLAUDE_CONTAINER_REWRITE_HOOKS=1` is set, the entrypoint drops the
`user` tier (to suppress cross-arch host hooks; see "Hooks" below) and
instead writes a project-tier `.mcp.json` inside
`${CLAUDE_HOST_PROJECT_DIR}` that mirrors the host's `mcpServers` with
each `command` wrapped in `exec-hook`. Run `/mcp` to see what loaded.

**Common bridged MCP servers**:

- **HTTP-bridge for cross-arch MCP binaries** —
  `CLAUDE_MCP_HTTP_BRIDGE=name=url:other=url` rewrites a stdio MCP
  server entry to Claude Code's native HTTP transport, so the
  in-container claude dials a host-side adapter (e.g.
  `http://host.docker.internal:8765/mcp`) instead of trying to exec a
  cross-arch binary. The host adapter is the operator's responsibility
  (`mcp-proxy`, `mcphost`, etc.); the container only rewrites the
  in-container `.mcp.json`. Full surface in
  [container/README.md](https://github.com/hndrewaall/claude-watch/blob/main/container/README.md#blast-radius).
- **`host-bash`** — generic "run a safe command on the host" MCP server,
  shipped as an off-the-shelf
  [`cli-mcp-server`](https://github.com/MladenSU/cli-mcp-server) +
  [`mcp-proxy`](https://github.com/sparfenyuk/mcp-proxy) combo with an
  env-var-driven allow-list. Default allow-list:
  `ls,cat,pwd,git,gh,head,tail,grep,find,echo`, no shell operators,
  `$HOME` boundary, 30s timeout. **If you need to run a command on the
  host and `host-bash` is available, use it** — that's exactly its
  purpose. If it's not available (`/mcp` doesn't list it), the operator
  hasn't wired up the host-side launcher. See
  [examples/compose/bin/mcp-host-bash](https://github.com/hndrewaall/claude-watch/tree/main/examples/compose/bin).

If `/mcp` shows "No MCP servers configured" inside the container, either
`CLAUDE_CONTAINER_REWRITE_HOOKS` is off (so user-tier MCP discovery is
suppressed by-default — the host's `mcpServers` simply don't load), or
the host's `~/.claude.json` has none defined.

## Hooks

The container ships [`exec-hook`](https://github.com/hndrewaall/claude-watch/blob/main/container/hooks-shim/exec-hook),
a safe-exec wrapper for `settings.json` hook commands whose target
binary may not be Linux-native. It inspects magic bytes, exec's ELF /
shebang-script targets transparently, and silently no-ops on Mach-O /
unknown formats with a single stderr heads-up per target per container
lifetime (so cross-arch hook references don't spam the log on every
event).

When `CLAUDE_CONTAINER_REWRITE_HOOKS=1`, the entrypoint generates a
container-local copy of `~/.claude/settings.json` with every hook command
wrapped in `exec-hook` and launches claude with
`--setting-sources project,local --settings /tmp/claude-shim/settings.json`
so the host file is never mutated.

**Realistic hook fate inside the container** (per hook event type):

| Target binary | Fate | Notes |
| --- | --- | --- |
| Linux-native ELF | exec'd transparently | Behaves identically to no shim. |
| `#!/usr/bin/env <interpreter>` shebang script | exec'd transparently | Standard scripts (Python, Bash, Node) work fine. |
| macOS Mach-O / Windows PE / unknown | silent no-op, exit 0 | One stderr line per unique target path per container lifetime. |
| Missing file | silent no-op, exit 0 | Same dedup behavior. |

**Implication for corporate telemetry hooks**: a Mac-host telemetry
binary referenced from `~/.claude/settings.json` (typical pattern: under
`~/.devbar/bin/` or similar) **does not fire inside the container**.
exec-hook detects the Mach-O and silently no-ops, intentionally — the
alternative ("Exec format error" on every hook event) is worse. If your
team requires telemetry from container sessions, the options are:

1. Ship a Linux-amd64 build of the hook binary and bind-mount it at the
   same path the host config references. (Coordinate with the team that
   owns the hook.)
2. Bridge the hook event over `host-bash` MCP — the in-container claude
   could invoke the host-native binary via the host-bash bridge at
   session-start. This is a per-team plumbing exercise, not built in.
3. Accept that in-container sessions are not telemetered into the host's
   pipeline. Coordinate with your team's privacy / observability stance.

The container does **not** carry corp telemetry pipelines into a
sandboxed Linux environment by default — that's an explicit design
choice. Make this decision with your team.

**Verifying hooks are reaching the right fate**: with
`CLAUDE_CONTAINER_REWRITE_HOOKS=1` and `verbose=true` in settings.json,
Claude Code logs each hook invocation. exec-hook writes its
"skipped non-ELF hook" heads-up to stderr on first occurrence per target
path. Tail `/tmp/exec-hook-skipped` inside the container for the list of
skipped binaries (one line per target).

## Workflow boundaries

This Claude Code session runs inside an isolated container. Its strengths
and limits:

- **Strong fit**: writing code in `${CLAUDE_HOST_PROJECT_DIR}`, talking
  to APIs the operator has bridged in (corp gateways via mcp-adaptor,
  off-the-shelf MCP servers, the Anthropic API). All TLS chains terminate
  at the in-container Node / Python; corporate-CA bundles forward
  through `NODE_EXTRA_CA_CERTS` etc. when the operator wires them up.
- **Weak fit**: anything that requires the host's full toolchain, the
  host's keychain, or commands not on the `host-bash` allow-list. Use
  `host-bash` (when available) for those — its allow-list is
  intentionally conservative.
- **Not in scope**: managing services on the host machine itself. If you
  need to restart a host daemon, edit host cron, or touch a host service,
  ask the operator on their host session; the container is a code-writing
  sandbox, not a host-administration tool.

## Quick reference for common in-container surprises

- **`claude` resumes a prior conversation**: when `CLAUDE_AUTO_CONTINUE`
  is set, the entrypoint appends `--continue <value>` to the claude
  invocation. Default is unset (bare `claude`).
- **`session-task`, `claude-event`, `obligations` on PATH**: only when
  the operator bind-mounts `~/repos/claude-watch` (the example compose
  does this). Missing bind mount = these CLIs are unavailable; that's
  expected for a stripped-down `docker run`.
- **Permission denied writing into `${HOME}/.local/share/claude/`**:
  the in-container claude binary's auto-update path. Backed by a named
  volume (`claude-container-versions`); should Just Work after the
  one-shot Dockerfile chown. If it doesn't, check that the named volume
  is mounted (the example compose does this) and that uid 1000 owns it.
- **`tmux` session is `claude-container:0.0`** — not `dashboard:main`
  like a typical host install. claude-watch's in-container config pins to
  this session name.

## Where to learn more

- [Top-level claude-watch README](https://github.com/hndrewaall/claude-watch/blob/main/README.md)
- [container/ README](https://github.com/hndrewaall/claude-watch/blob/main/container/README.md) — full Dockerfile / entrypoint / blast-radius reference
- [examples/compose/ README](https://github.com/hndrewaall/claude-watch/blob/main/examples/compose/README.md) — fresh-laptop developer stack walkthrough
- [Claude Code memory docs](https://code.claude.com/docs/en/memory) — canonical CLAUDE.md hierarchy reference
- [Claude Code hooks docs](https://code.claude.com/docs/en/hooks) — full hook event list + exit-code semantics
