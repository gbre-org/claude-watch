# IDE-mode injection — protocol findings (May 2026)

## Summary

claude-watch's interruption mechanism (`tmux send-keys` into the monitored
pane) reaches Claude Code when the agent runs in **terminal mode** (the
default, when launched with `claude` in any terminal, including the
integrated terminal of any IDE that is attached to a tmux session). It does
**not** reach Claude Code when the agent runs in **IDE panel mode** (the
extension's native chat UI, when `claudeCode.useTerminal=false`).

This document records the protocol investigation done to determine whether
a second injection path (alongside `tmux send-keys`) could be added to
reach panel-mode agents.

**Two passes**:

1. **VSIX-string pass (initial)** — concluded *no out-of-process inject
   path exists*. That conclusion was reached by reasoning from string
   constants in the bundled extension JS, not by running a live agent. It
   was wrong.
2. **Empirical re-probe (2026-05-16)** — spawned a live panel-mode-shape
   agent, observed its file descriptors and syscalls, and found an inject
   path that the string pass missed. See § "EMPIRICAL re-probe 2026-05-16"
   below. The string-pass conclusion is **superseded** for the inject-path
   question; the rest of the protocol surface (what the SSE port does, how
   the chat UI is wired) was correctly characterised and is retained
   verbatim.

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

**Superseded by the empirical re-probe (2026-05-16).** See § "EMPIRICAL
re-probe 2026-05-16" below — an out-of-process inject path DOES exist via
Linux `pidfd_getfd(2)`. The text below describes what the string pass
incorrectly concluded; it is kept for historical reference and for the
non-inject fallback ladder (events / MCP polling / URI handler) which are
still useful when the pidfd path can't be used (different uid, locked-down
`ptrace_scope`, non-Linux host, etc.).

(Historical string-pass conclusion, **superseded** by the empirical pass:)

> No out-of-process injection path exists. claude-watch cannot deliver
> mid-generation text into a panel-mode agent because:
>
> - The agent process has no listening socket.
> - The stdin pipe is owned by the extension's Node process and is not
>   reachable from another process. `/proc/<pid>/fd/0` for the agent points
>   at the read-side of the pipe, which is correctly readable but not
>   writable — only the extension holds the write end.
> - The MCP-IDE server hosted by the extension is for tool calls
>   (agent → IDE), not user input.
> - The webview-bridge is an IDE-internal IPC channel with no external
>   surface.

The non-inject fallback ladder remains a useful reference for the cases
where pidfd inject is unavailable:

| Approach | What it delivers | When | Latency |
|---|---|---|---|
| claude-event drop | Line in next `UserPromptSubmit` context | After current turn finishes | Bounded by current turn duration |
| Custom MCP server with `panic_button` tool | Agent polls tool periodically | On next turn-internal tool call | Up to one turn-tool-call cycle |
| `Notification` hook event | Tool-call hook fire | On next tool call | Up to one tool call |
| URI handler `vscode://...?prompt=...` | New tab pre-filled with prompt text | When user clicks/opens the tab | User-driven |

None of these are equivalent to `tmux send-keys` (which cancels the
in-flight generation by sending Escape + typing) — and neither is the
pidfd inject path: it appends a user message to the agent's stream-json
stdin, which the agent processes on its next event-loop tick, but it does
NOT cancel an in-flight model generation. For cancellation, only the
user-initiated stop button in the webview UI has the needed effect; that
button has no external API.

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

## Verdict (string-pass — superseded)

There is **no MCP-server-side hook**, **no chat-input HTTP endpoint**, and
**no filesystem channel** on the claude process that claude-watch can use
to inject user-input text into a panel-mode agent. Adding an
`sse_inject` codepath would require shipping a fake protocol that doesn't
exist on the wire.

**This verdict is wrong for the inject question.** It is correct that the
MCP-IDE server is for tool calls and not user input, and that there's no
HTTP endpoint or filesystem channel — but the string pass missed Linux's
`pidfd_getfd(2)` route to the extension host's matching socketpair
endpoint. See the empirical re-probe below.

The tmux-inject codepath remains correct for every deployment that runs
claude in a tty (which includes the workbot container today). Panel-mode
users now have two options: events / obligations (cross-platform, latency
bounded by next turn) and pidfd inject (Linux only, latency = next
event-loop tick).

## EMPIRICAL re-probe 2026-05-16

### Trigger

Andrew pushed back on the string-pass conclusion: image-paste in panel
mode demonstrably gets data from the webview UI into the running agent
without going through the integrated terminal, so *some* channel must
exist. The right response was to run a live agent and inspect what it
actually opens / reads / writes, not to re-read more VSIX strings.

### Setup

Spawned a `claude --output-format stream-json --verbose --input-format
stream-json --permission-mode bypassPermissions` process from a small
Node wrapper that mimics the extension's `child_process.spawn(..., {stdio:
['pipe','pipe','pipe'], env: {...CLAUDE_CODE_SSE_PORT='0'...}})` call.
This reproduces the panel-mode shape exactly (same SDK transport, same
piped stdio, same env vars) on a host where no real VSCode extension is
running. The same `@anthropic-ai/claude-agent-sdk` code path serves both
the real extension and this test wrapper.

### Findings

1. **`stdio: 'pipe'` in Node is AF_UNIX SOCK_STREAM, not anonymous pipes.**
   `ls -la /proc/<agent-pid>/fd/` shows `0 -> socket:[N]`, `1 -> socket:[N+2]`,
   `2 -> socket:[N+4]`. `/proc/net/unix` lists each as type 0001
   (`SOCK_STREAM`), state 03 (`CONNECTED`), with no pathname (anonymous
   socketpair endpoints — `socketpair(AF_UNIX, SOCK_STREAM, ...)` allocates
   the inode pair with consecutive numbers).

2. **`/proc/<agent-pid>/fd/0` is NOT openable from outside.** Tried `echo
   X > /proc/PID/fd/0` (same uid), `sudo tee /proc/PID/fd/0`, `dd
   of=/proc/PID/fd/0`. All three fail with `ENXIO` ("No such device or
   address"). This is a kernel restriction: `open(2)` on the `/proc/fd/N`
   magic symlink for an anonymous AF_UNIX endpoint returns ENXIO because
   the socket has no pathname to bind / connect through.

3. **The agent has NO listening socket.** Walked the full fd list and
   cross-referenced `/proc/net/tcp` + `/proc/net/tcp6`. The only IPv4/IPv6
   sockets the agent owns are outbound TCP connections to the Anthropic
   API. There is no localhost listener — the agent is purely an MCP
   *client* of the extension's SSE port; it is never an input *server*.

4. **`strace -p <agent-pid>` shows input arrives via `recvfrom(0, ...)`.**
   With the agent in its normal `epoll_wait` sleep, sending a stream-json
   line via the parent's stdin pipe wakes the agent and the next syscall
   is `recvfrom(0, "{\"type\":\"user\",...PROBE...}", 262144, MSG_DONTWAIT,
   NULL, NULL) = N`. The fd is 0 (stdin), the syscall is `recvfrom`
   (proves it's a socket, not a pipe), and the bytes are the full NDJSON
   line. No separate input fd. Image-paste flows the same channel as
   plain text: the webview encodes the image as base64, posts to the
   extension via `webview.postMessage`, the extension wraps it in a
   stream-json `user` message with a `{"type":"image","source":{"type":
   "base64","media_type":...,"data":...}}` content block, and writes the
   resulting NDJSON line to the same stdin socket. No side channel.

5. **`pidfd_getfd(2)` DOES dup the parent's matching socketpair end.**
   On Linux 5.6+ with `kernel.yama.ptrace_scope = 0` (default Debian
   desktop), a same-uid process can `pidfd_open(parent_pid)` then
   `pidfd_getfd(pidfd, parent_fd)` to obtain a writable copy of the
   parent's stdin-write-end. Writing a stream-json line to that dup'd fd
   places the bytes in the agent's stdin receive queue, where the agent
   picks them up via `recvfrom` on its next event-loop tick. Tested
   live: three sequential injects produced three sequential responses
   ("ready" / "ok" / "yes") from the agent's stdout. Total kernel write
   round-trip = ~5–6 KB of streamed JSON per inject (init events +
   assistant message + result block).

   Discovery: the matching parent-side fd is the one whose socket inode
   equals `agent_fd0_inode - 1`. Linux's `socketpair()` allocates the two
   endpoints with consecutive inode numbers; the parent-side end gets the
   lower of the two (the parent keeps its end and dup's the child's end
   into the child's fd 0 during fork+exec). Walking
   `/proc/<parent-pid>/fd/*` for a socket fd whose readlink target
   matches the expected inode finds it in one pass.

### Why the string pass missed this

The string pass focused on user-space protocols: SSE / WebSocket / HTTP /
MCP. It correctly identified that none of those carry user-input text
into the agent. What it missed was that the **kernel-level fd plumbing**
between two same-uid processes is itself an injection surface on Linux —
`pidfd_getfd` was merged in Linux 5.6 (March 2020) specifically to
support exactly this kind of "give me a handle to a fd you have, that I
otherwise couldn't open" pattern. The check the string pass should have
made is "does the same-uid attacker model expose any of the agent's fds
via kernel APIs" — not just "does the user-space protocol carry inject".

### What ships in this PR

- `src/inject_probe.rs` — implementation of the pidfd-inject path:
  - `parse_socket_inode` / `expected_parent_inode` — pure helpers.
  - `agent_stdin_socket_inode` / `parent_pid` / `find_parent_stdin_fd` —
    /proc scanners.
  - `build_user_message` — stream-json NDJSON builder.
  - `linux_inject::inject` — the `pidfd_open` + `pidfd_getfd` + `write`
    sequence (Linux-only, cfg-gated).
  - `probe` — end-to-end function returning `ProbeOutcome`
    (`Ok` / `WrongMode` / `AgentUnreadable` / `ParentFdNotFound` /
    `SyscallFailed`).
  - `cmd_inject_probe` — CLI handler with text + JSON output modes.
- `claude-watch inject-probe --pid <agent-pid> --text <text> [--json]`
  CLI subcommand. Exit codes: `0` = Ok, `1` = lookup / syscall failure,
  `2` = WrongMode (use tmux-inject instead).
- Unit tests covering inode parsing, payload shape, the saturating-sub
  edge case, and the self-pid / bogus-pid sad paths.

### What does NOT ship in this PR

- **No daemon-loop integration.** `policy.rs` is unchanged; this is a
  manual probe at this stage. Wiring inject-mode selection into the
  interrupt policy (terminal → tmux, panel → pidfd, otherwise → event)
  is a follow-up so the inject path can be reviewed in isolation first.
- **No prompt-injection cancellation.** The pidfd inject appends a user
  message to the stream-json stdin; the agent processes it on its next
  event-loop tick. It does NOT cancel an in-flight model generation
  (`tmux send-keys` sends Escape, which the panel UI does not). For
  cancellation, panel-mode agents are still bounded by the next natural
  turn boundary.
- **No cross-uid / locked-down ptrace_scope support.** The probe returns
  `SyscallFailed{stage: "pidfd_open"}` with `EPERM` in those cases. The
  fallback ladder (events / obligations) covers those deployments.

### Reproduction

```bash
# Spawn a panel-mode-shape agent (e.g. via the SDK)
node -e '...spawn claude with stdio:pipe stream-json...'

# Inspect fds
ls -la /proc/<agent-pid>/fd/0   # → socket:[N], not /dev/pts/M, not pipe:[N]

# Try /proc/fd/0 inject — will fail
echo '{"type":"user",...}' > /proc/<agent-pid>/fd/0   # → ENXIO

# Run the pidfd-inject probe
claude-watch inject-probe --pid <agent-pid> --text "hello from outside"
# → ok: wrote N bytes via pidfd_getfd(parent_pid=P, parent_fd=F)
```
