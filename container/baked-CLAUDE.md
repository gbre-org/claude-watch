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

## Session-start checklist — MANDATORY first action

**ON EVERY SESSION START (including `/clear`, restart, or context
compaction): run this checklist BEFORE doing anything else.** The whole
point is to surface what the container exposes — and what it doesn't —
in this session, so the rest of the conversation doesn't drift into
assumptions about a host-side surface that isn't here.

This is the container equivalent of a host-side "resume checklist".
The list is intentionally short — the container is a sandbox for code
work, not the host's full automation stack, so the checks below are
all that's needed.

1. **Self-id**: run `cat /etc/claude-code/CLAUDE.md | head -3`. Confirm
   you see the "claude-container — runtime environment" header. If you
   don't, you are NOT in this container — stop and re-check before
   continuing (some host-side instructions are unsafe to run in a
   container; some container-side ones are unsafe on the host).
2. **MCP bridges reachable**: run `claude mcp list`. Expected to see at
   least `mcp-adaptor` and (if the operator configured it) `host-bash`,
   each with a `Connected` status. If a bridge shows as failed, note it
   for the operator — many corp workflows depend on these.
3. **Hook fate**: run `audit-hooks` (no args). The summary line reports
   how many host-bound hooks land as `ok-elf`/`ok-script` vs
   `silent-no-op`/`missing`. A non-zero `silent-no-op` count is normal
   for cross-arch host (e.g. Mac) telemetry binaries — that's
   `exec-hook` doing its job. The check is informational; you don't have
   to act on it unless the operator asks.
4. **Probe host OS via `host-bash`** (if `host-bash` is connected): a
   single `uname -s` (or `powershell -Command "$PSVersionTable.OS"` if
   `uname` isn't present) tells you whether the host is Linux, macOS,
   or Windows. The answer shapes which scheduler / package-manager
   guidance below applies. Skip if `host-bash` is unavailable.
5. **Announce scope**: in your first response of the session, state
   one line summarizing where you're running (claude-container, the
   bind-mount surface from the table below, MCP bridges available, hook
   audit summary, host OS if probed) so the operator can see at a
   glance what you have to work with. Keep it concise — one or two
   sentences.
6. **List baked skills + agents + watchers**: `ls
   /etc/claude-code/skills/ /etc/claude-code/agents/
   /etc/claude-code/watchers/`. Skills land at
   `/claude-container:<name>` (e.g. `/claude-container:restart`,
   `/claude-container:start-watchers`); agents are spawned with
   `Agent(subagent_type="claude-container:<name>", ...)`; watchers are
   shell scripts the agent launches via the `Bash` tool with
   `run_in_background: true`. The full convention + how-to-add lives in
   the per-dir READMEs at the repo's
   [`container/skills/`](https://github.com/hndrewaall/claude-watch/tree/main/container/skills),
   [`container/agents/`](https://github.com/hndrewaall/claude-watch/tree/main/container/agents),
   [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers).

**There are no long-running watchers inside this container.** This is
deliberate — the container is a code-writing sandbox, not a host
automation hub. Don't try to start signal watchers, torrent watchers,
podcast watchers, or anything else from the host's resume-checklist
playbook; the relevant tools and services aren't installed here.

If the operator gives you a job that genuinely needs a host-side
watcher / notifier, run it on the host instead (via the operator's host
Claude Code session) or bridge the watch event over `host-bash`.

## Avoid `sudo` — fingerprint prompt is prohibitive

On the operator's host (typically macOS), every `sudo` invocation
triggers a Touch ID / fingerprint prompt. That's prohibitive when an
agent loop chains many short commands, so **prefer non-sudo paths in
this container whenever possible**.

The container user is uid 1000 (`hndrewaall`) and is in the right
groups (including `docker`, where applicable) so the following commands
**never need `sudo`** inside the container:

- `docker compose ...` — when docker socket is bind-mounted, the
  container user has docker-group access; bare `docker compose` works.
- `git` — repo trees are bind-mounted with the container user as
  owner; `git status`, `git diff`, `git log` etc. don't need root.
- `claude`, `claude-watch`, `claude mcp ...`, `claude-event`,
  `session-task`, `obligations`, `agent-msg` — all run as the
  container user.
- `npm`, `yarn`, `pnpm`, `node`, `cargo`, `rustc`, `python`, `pip`,
  `uv`, `go`, `make` — language toolchains run as the container user.
- `audit-hooks`, `trust-workspace` — container-baked helpers, both
  run as the container user.

If you find yourself wanting `sudo` for something that isn't on this
list (e.g. `apt install`, writing to `/etc/`, editing a system service
unit), **pause and ask the operator first**. The fingerprint prompt
makes silent retries painful, and most "I need sudo" instincts inside
the container are a sign of either a missing bind-mount or a
container-vs-host confusion that's better resolved by talking to the
operator than by working around it.

The lone documented exception is the `cw` host shim referenced in
`examples/compose/bin/cw`, which falls back to `sudo docker` only if
bare `docker ps` fails on the host. That fallback runs on the host,
not in the container, and is a one-time setup decision the operator
made about their host docker permissions — not a pattern the
container session should imitate.

## Self-update — `cwsr` rolls the inner `claude` without container restart

When Anthropic ships a new `@anthropic-ai/claude-code` version, you do
NOT need the operator to `docker compose restart` the whole container
to pick it up. Run `cwsr` (in-container; baked at
`/usr/local/bin/cwsr`) and the inner claude rolls in-place:

```sh
cwsr                    # npm install -g @latest, then respawn pane 0
cwsr --version 2.1.150  # pin a specific npm version
cwsr --no-upgrade       # respawn current claude (rare; for testing)
cwsr --upgrade-only     # install without rolling (operator can `cwsr --no-upgrade` later)
cwsr --print            # dry-run; print planned NPM + TMUX argv
```

What survives the roll: the tmux session (`claude-container:0.0`), the
wrapping container, every MCP bridge that was up, the named-volume
`~/.local/share/claude/versions/` directory, the operator's tmux
attach. What rolls: the claude process inside pane 0.

When you should run `cwsr`:
- The operator says "upgrade to latest" or asks you to pick up a
  specific version they reference.
- You see (e.g. via `claude --version`) that the in-container version
  has fallen behind a release the operator wants.

When `cwsr` is NOT the right tool:
- Container itself is down — use `docker compose up -d` (or `cw --up`
  from the host); that path installs the freshest baked version.
- You need to change `CLAUDE_AUTO_CONTINUE`, `CLAUDE_CONTAINER_REWRITE_HOOKS`,
  `CLAUDE_HOST_PROJECT_DIR`, or any other entrypoint-time env var —
  those decisions are baked at container start; cwsr only rolls the
  inner process with whatever shape entrypoint.sh already chose. Ask
  the operator to `docker compose up -d --force-recreate` for those.

The package name (`@anthropic-ai/claude-code`) and install command
(`npm install -g`) are cross-platform — same shape works whether the
host is Linux, macOS, or Windows. The in-container npm itself runs as
uid 1000 against a writable global path, no sudo needed.

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
  env-var-driven allow-list. Default allow-list (`CW_PROFILE=corp-dev`,
  the conservative read-only set):
  `ls,cat,pwd,git,gh,head,tail,grep,find,echo`, no shell operators,
  `$HOME` boundary, 30s timeout. Trust-profile `CW_PROFILE=corp-dev-trusted`
  widens this with host-scheduling tooling (see the
  "Host-side scheduled tasks" section below). **Reach for host-bash as
  a normal tool, not a last resort** — it's the supported way to do
  host-side work from inside the container. If it's not available
  (`/mcp` doesn't list it), the operator hasn't wired up the host-side
  launcher. See
  [examples/compose/bin/mcp-host-bash](https://github.com/hndrewaall/claude-watch/tree/main/examples/compose/bin).

  **Boundary discipline**: host-bash is a *window* to the host, not
  the host. When you report what you did, frame it as "I ran X on the
  host via host-bash" — not "I ran X" (ambiguous) and not "I'm on the
  host" (false; you remain inside the container the whole time). The
  in-container claude orchestrates host-side work; the host-side
  shell executes it. Keep that distinction crisp in self-reports so
  the operator never has to guess where a command actually ran.

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

## Host-side scheduled tasks (via `host-bash`)

The container has no built-in cron / launchd / systemd — it's a
sandbox for code work, not a host automation hub. But the operator
sometimes needs periodic work that fires events the in-container claude
reacts to (e.g. "every 10 minutes, check Slack for tags addressing
me and reply if needed"). The supported pattern is:

1. **In-container claude orchestrates the schedule definition** via
   `host-bash` — edit a small script under `~/repos` (which is
   bind-mounted into the container) and use the host's native
   scheduler to fire it on a cadence.
2. **The host-side script writes a `claude-event` JSON** (or whatever
   signaling mechanism the container has wired up for inbound events)
   into a bind-mounted path. The container picks it up on its next
   pass.
3. **In-container claude reacts** to the event when it surfaces.

This requires `CW_PROFILE=corp-dev-trusted` (or an operator-specified
ALLOWED_COMMANDS override) so `host-bash` will actually exec the
scheduler binaries. The conservative default profile blocks them on
purpose — opt in.

### Host OS detection (always do this first)

The host could be **Linux** (cron, systemd user timers), **macOS**
(launchd via `launchctl`), or **Windows** (Task Scheduler via
`schtasks` / `Register-ScheduledTask`). Don't assume — probe via
host-bash before reaching for any specific scheduler:

```sh
# host-bash run_command:  uname -s
#   → "Linux"     → cron / systemd
#   → "Darwin"    → launchd
#   → "MINGW*" / "MSYS*" / "CYGWIN*" / "Windows_NT" → Task Scheduler
```

If `uname` isn't available (Windows without WSL), try
`powershell -Command "$PSVersionTable.OS"` or
`schtasks /Query /TN \\` as a probe.

### Worked example: periodic Slack tag-check

Operator wants the in-container claude to check Slack every 10 minutes
for messages tagging them and reply if needed. The orchestration:

1. **In-container claude** writes
   `~/repos/<some-host-accessible-path>/check-slack-tags.sh` — a small
   script that calls the operator's slack CLI on the host, looks for
   tags, and emits a `claude-event` if any are found.
2. **In-container claude** uses host-bash to wire that script into the
   host's scheduler. Pseudocode per host OS:

   ```sh
   # Linux (cron):
   #   echo "*/10 * * * * /home/$USER/repos/check-slack-tags.sh" \
   #     | host-bash crontab -

   # macOS (launchd, user agent):
   #   host-bash tee ~/Library/LaunchAgents/com.local.slack-tag-check.plist <<'EOF'
   #   <plist>... StartInterval 600 ... ProgramArguments slack-tag-check.sh ...</plist>
   #   EOF
   #   host-bash launchctl load -w ~/Library/LaunchAgents/com.local.slack-tag-check.plist

   # Windows (Task Scheduler):
   #   host-bash schtasks /Create /TN "ClaudeSlackTagCheck" \
   #     /TR "C:\path\to\check-slack-tags.bat" /SC MINUTE /MO 10
   ```

   (Actual scheduler argv depends on the host. The above is the
   *shape*. Use the OS probe to pick which branch to run.)
3. **The script** emits `claude-event` via the bind-mounted path that
   the in-container watcher infrastructure consumes.
4. **In-container claude** picks up the event on its next pass.

### Always document the dismantle

A scheduled job is durable on the host long after the container
session ends. Whenever you wire one, document the dismantle command in
the same conversation (so the operator can clean up):

```sh
# Linux:   host-bash crontab -l | grep -v slack-tag-check | host-bash crontab -
# macOS:   host-bash launchctl unload -w ~/Library/LaunchAgents/com.local.slack-tag-check.plist
#          host-bash rm ~/Library/LaunchAgents/com.local.slack-tag-check.plist
# Windows: host-bash schtasks /Delete /TN "ClaudeSlackTagCheck" /F
```

### Boundary reminder

Host-side schedulers are running on **the host**, not in the
container. The container is the orchestrator: it writes the
definition files (via host-bash), it consumes the resulting events,
but the cron / launchd / systemd / Task Scheduler process itself
lives outside. When reporting "I set up a recurring Slack check",
frame it as "I wrote a host-side <scheduler> job that fires every N
minutes" — not "I'm running every N minutes" (the container session
isn't; the host scheduler is).

## Where to learn more

- [Top-level claude-watch README](https://github.com/hndrewaall/claude-watch/blob/main/README.md)
- [container/ README](https://github.com/hndrewaall/claude-watch/blob/main/container/README.md) — full Dockerfile / entrypoint / blast-radius reference
- [examples/compose/ README](https://github.com/hndrewaall/claude-watch/blob/main/examples/compose/README.md) — fresh-laptop developer stack walkthrough
- [Claude Code memory docs](https://code.claude.com/docs/en/memory) — canonical CLAUDE.md hierarchy reference
- [Claude Code hooks docs](https://code.claude.com/docs/en/hooks) — full hook event list + exit-code semantics
