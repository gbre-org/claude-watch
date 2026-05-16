# VSCode IDE-mode injection — protocol findings (May 2026)

## Summary

claude-watch's interruption mechanism (`tmux send-keys` into the monitored
pane) reaches Claude Code when the agent runs in **terminal mode** (the
default, when launched with `claude` in any terminal, including VSCode's
integrated terminal that is attached to a tmux session). It does **not**
reach Claude Code when the agent runs in **VSCode panel mode** (the
extension's native chat UI, when `claudeCode.useTerminal=false`, which is
the install default).

This document records the protocol investigation done to determine whether
a second injection path (alongside `tmux send-keys`) could be added to
reach panel-mode agents. The investigation concluded that **no
out-of-process injection path exists** — the extension owns the only input
channel into the agent process. The "SSE inject" hypothesis floated during
earlier probe research was based on a misreading of what
`CLAUDE_CODE_SSE_PORT` does.

## What the SSE port actually is

When the VSCode extension activates, it starts an in-process **MCP server**
on a random high port bound to `127.0.0.1`. The server's purpose is to
expose IDE features as MCP tools that the agent can call:

- `mcp__ide__getDiagnostics` — returns the Problems-panel entries from the
  workspace's language servers.
- `mcp__ide__executeCode` — runs Python in the active Jupyter notebook's
  kernel (with a user-facing confirmation prompt).

Plus a dozen internal RPCs the CLI uses for its own UI (open-diff,
read-selection, save-file, etc.) which are filtered from the tool list the
agent sees.

The port + auth token are written to a per-extension-activation lockfile
under `~/.claude/ide/<port>.lock` with 0600 permissions in a 0700
directory. The CLI also sets `CLAUDE_CODE_SSE_PORT` in the env of the
claude process it spawns, so the client side can locate the server without
re-reading the lockfile.

**Direction of communication.**

- The extension is the **MCP server**.
- The claude agent is the **MCP client**.
- The wire protocol is JSON-RPC 2.0 over either WebSocket (`ws-ide`,
  preferred) or HTTP+SSE (`sse-ide`, legacy).
- Requests flow agent → IDE (e.g. "get diagnostics for this file"). The
  IDE responds inline.
- Notifications flow IDE → agent (e.g. `selection_changed`, `tab_changed`,
  `at_mentioned_files`). These surface workspace events in the agent's
  next turn.

What this port is **not**: it is not a user-input endpoint, not a chat
surface, and not a way to push prompt text into the running agent. The
"VSCode-side chat UI talks to the agent over the SSE port" assumption that
motivated this investigation was incorrect.

## What the chat UI actually does

In VSCode panel mode (the default chat-UI experience), the extension:

1. Spawns the claude binary as a subprocess with
   `stdio: ["pipe","pipe","..."]` — i.e. stdin/stdout pipes owned by the
   extension Node process.
2. Renders the chat UI inside a webview.
3. On user submit, the webview posts a `message` to the extension via
   `webview.postMessage(...)`.
4. The extension handler writes the message bytes into the agent
   subprocess's stdin pipe.
5. Agent output is read from stdout, parsed, and rendered back to the
   webview.

In short: **user input → webview → extension JS → agent stdin**. There is
no localhost HTTP server hosted by the agent, no socket, and no
filesystem-based input channel. The only handle on the input end is the
stdin file descriptor held by the extension's Node process.

Terminal mode (`claudeCode.useTerminal=true`, or `claude` invoked from a
shell — including VSCode's integrated terminal) is different: claude runs
in a pty under the user's shell, and stdin is the terminal. That is the
mode `tmux send-keys` targets.

## Implications for claude-watch

### Terminal mode (workbot today, native CLI users)

`tmux send-keys` into the claude pane is the correct mechanism and
continues to work unchanged. No new code path is needed for this
deployment. The interruption hierarchy (events < obligations <
interruptions) operates as designed.

For workbot specifically: `claude` runs inside a tmux session inside the
container; the user (or VSCode's integrated terminal) attaches to that
session. claude-watch's existing tmux-inject path reaches the agent
correctly. The premise that "tmux-inject is silently a no-op for workbot"
does **not hold** — it would only hold if Andrew switched workbot to use
the VSCode native chat panel instead of the integrated terminal + tmux
setup.

### Panel mode (extension's native chat UI)

No out-of-process injection path exists. claude-watch cannot deliver
mid-generation text into a panel-mode agent because:

- The agent process has no listening socket.
- The stdin pipe is owned by the extension's Node process and is not
  reachable from another process. `/proc/<pid>/fd/0` for the agent points
  at the read-side of the pipe, which is correctly readable but not
  writable — only the extension holds the write end.
- The MCP-IDE server hosted by the extension is for tool calls
  (agent → IDE), not user input.
- The webview-bridge is a VSCode-internal IPC channel with no external
  surface.

The available fallbacks all share the same limitation: they cannot
**interrupt** the agent mid-generation. They can only deliver context that
the agent will see on a turn boundary it reaches on its own.

| Approach | What it delivers | When | Latency |
|---|---|---|---|
| claude-event drop | Line in next `UserPromptSubmit` context | After current turn finishes | Bounded by current turn duration |
| Custom MCP server with `panic_button` tool | Agent polls tool periodically | On next turn-internal tool call | Up to one turn-tool-call cycle |
| `Notification` hook event | Tool-call hook fire | On next tool call | Up to one tool call |
| URI handler `vscode://...?prompt=...` | New tab pre-filled with prompt text | When user clicks/opens the tab | User-driven |

None of these are equivalent to `tmux send-keys` (which cancels the
in-flight generation by sending Escape + typing). The only generation
cancel path inside panel mode is a user-initiated stop button in the
webview UI, which has no external API.

### Recommended product shape

claude-watch's "interruption" tier remains scoped to terminal-mode
deployments. For panel-mode users, the right intervention tier is **events
or obligations** — both of which already work cross-deployment because
they live in the agent's own filesystem (`~/claude-events/`,
`obligations` CLI) rather than depending on input-channel injection.

Concretely:

- A near-limit context warning that today fires `tmux send-keys` should
  detect deployment mode (presence of `CLAUDE_CODE_SSE_PORT` in the
  monitored agent's env = panel mode) and either:
  - Skip the tmux inject + emit a louder claude-event (panel mode), or
  - Continue with tmux inject as today (terminal mode).
- For panel mode, escalate via the existing obligation/event tiers; the
  agent picks the signal up on its next turn boundary.

This is a smaller change than building an SSE-inject path that doesn't
exist on the wire.

## How to detect deployment mode

Inspect the env of the monitored claude process:

```
tr '\0' '\n' < /proc/<claude-pid>/environ | grep -E '^CLAUDE_CODE_SSE_PORT='
```

- Present → panel mode (or some other IDE-integrated mode where the
  agent was spawned by an extension).
- Absent → terminal mode (claude was launched from a shell, possibly
  inside tmux). `tmux send-keys` is the appropriate channel.

Note: the integrated VSCode terminal with `/ide` connected will ALSO have
`CLAUDE_CODE_SSE_PORT` set (the CLI auto-connects to the extension's MCP
server when it detects VSCode), but in this mode claude is still a tty
process and `tmux send-keys` still works. Detection therefore needs a
second check: is the agent's stdin a pty (`/proc/<pid>/fd/0` resolves to
`/dev/pts/*`) or a pipe (resolves to `pipe:[...]`)? Pty → terminal mode,
pipe → panel mode.

## How the probe was conducted (May 2026)

1. **String inspection** of the bundled claude binary
   (`@anthropic-ai/claude-code-linux-x64/claude`). Found:
   - References to `CLAUDE_CODE_SSE_PORT`, `sse-ide`, `ws-ide`, `stdio`,
     `http` transports.
   - MCP tool names `mcp__ide__getDiagnostics`, `mcp__ide__executeCode`.
   - Notification kinds `selection_changed`, `tab_changed`,
     `at_mentioned_files`, etc.
   - IDE lockfile path discovery logic (`~/.claude/ide/<port>.lock`).
2. **VSIX unpacking** of `anthropic.claude-code` extension from the VS
   Marketplace. Found in `extension.js`:
   - `.listen(N, "127.0.0.1", ...)` followed by
     `MCP Server running on port ${N} (localhost only)` log line — this
     is the extension hosting the MCP server.
   - `aV0(V, "CLAUDE_CODE_SSE_PORT", String(N))` — extension sets the env
     var so child processes inherit the port.
   - `spawn(K, V, {cwd: ..., stdio: ["pipe","pipe", Z], ...})` — extension
     spawns the agent with piped stdio.
   - `webview.postMessage(...)` and webview message handlers — the input
     channel is the webview-to-extension bridge, not a network endpoint.
3. **No HTTP listen** of any input endpoint on the agent side. The agent
   binary calls `.listen()` only for the MCP-OAuth callback (a separate
   localhost callback for MCP server auth flows) and as MCP client setup
   for `http`/`sse`/`ws` transports, never as a chat-input server.

## Verdict

There is **no MCP-server-side hook**, **no chat-input HTTP endpoint**, and
**no filesystem channel** on the claude process that claude-watch can use
to inject user-input text into a VSCode-panel-mode agent. Adding an
`sse_inject` codepath would require shipping a fake protocol that doesn't
exist on the wire. The queue item's "don't ship a hack that fakes SSE if
the real protocol can't be cracked" guardrail therefore applies — the
real protocol IS cracked, and the answer is that the protocol does not
contain the surface the feature would need.

The tmux-inject codepath remains correct for every deployment that runs
claude in a tty (which includes the workbot container today). Panel-mode
users are best served by the events / obligations tiers rather than
interruptions.
