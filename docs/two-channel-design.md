# Two-channel design — orchestration tmux + rich-UX panel

> Status: design proposal, 2026-05-16; image-paste section added
> 2026-05-17 with empirical findings that contradict the original
> 2026-05-16 revision's claim that "image paste works via tmux
> passthrough" in the cw container shape. See § 3.7. No code changes
> accompany this doc.

## 1. Problem statement

claude-watch ships two ways to run Claude Code:

1. **Terminal mode** — the agent runs in a pty inside a tmux session. The
   daemon attaches to that pane, watches activity via `tmux capture-pane`,
   and intervenes via `tmux send-keys`. Orchestration features (mid-turn
   interrupt, fresh-session detection, resume-injection, zombie recovery,
   token-stall watchdog) all work because the daemon can see and reach the
   pane.
2. **IDE-panel mode** — the VSCode extension's native chat webview spawns a
   `claude --output-format stream-json` subprocess with piped stdio. There
   is no tmux pane to capture and no pty to `send-keys` into. The
   recently-merged [`inject_probe`](../src/inject_probe.rs) module proved
   that out-of-process injection IS possible via `pidfd_getfd(2)`, but the
   orchestration features that depend on tmux pane capture (activity
   detection, fresh-/clear, token-stall watchdog, zombie recovery) still
   only fire against terminal-mode agents.

The product goal is **one agent process per operator that gets BOTH**:

> "i want the benefits of tmux and while vscode is running communication w it"
> "no, thats what our tmux is for (in addition to mediating interrupt) it handles
> orchestration which combined w docker is enough for daemonization imo"
>
> — Andrew, 2026-05-16

In other words: keep the agent inside tmux (so claude-watch can see + reach
it), but also surface that same agent inside the operator's editor so they
get image paste, rich diff rendering, the @-mention picker, and so on
without managing two separate Claude Code conversations.

This document records the empirical investigation into whether that
single-agent shape is reachable today and, if so, how.

## 2. Current architecture

Two distinct shapes ship today and they are not interchangeable.

### 2.1 Terminal mode (workbot / cw container today)

```
operator's terminal (VSCode integrated panel, iTerm2, ssh, ttyd)
   │
   └─> cw  →  docker compose exec -it claude-container  →  pty (/dev/pts/N)
                 │
                 └─> tmux -u new-session -A -s claude-container
                        │
                        └─> existing tmux session
                              │  (created by the container entrypoint at
                              │   startup, headlessly: `tmux new-session -d`)
                              │
                              └─> window 0, pane 0
                                    │
                                    └─> claude (fd 0 → /dev/pts/M, no
                                                  CLAUDE_CODE_SSE_PORT in env)
                                          │
                                          └─> in-container claude-watch
                                              daemon (same container,
                                              same tmux server, scrapes
                                              pane 0)
```

Key properties:

- The tmux session is created by the container entrypoint, not by the
  operator's terminal. It exists from container startup, with no client
  attached.
- Each `cw` invocation just attaches the operator's terminal to the
  existing session (`tmux -u new-session -A` — attach if it exists,
  otherwise create). Closing the operator's terminal (or Ctrl-C-ing the
  outer `docker compose exec`) detaches but does not kill anything inside;
  the agent and its watcher daemon keep running. Tmux + docker is the
  daemonization layer, as Andrew put it.
- The agent's stdin is `/dev/pts/M`. `tmux send-keys` writes bytes there,
  which Claude Code processes as keyboard input (including Escape to
  cancel mid-generation).
- claude-watch's `proc_util::agent_deployment_mode` classifies this as
  `Terminal` (pty fd 0 + no CLAUDE_CODE_SSE_PORT). The
  `inject_dispatch::inject_to_agent` dispatcher routes interrupts through
  `tmux::inject_text` here, which is the historical correct path.

### 2.2 IDE-panel mode (VSCode extension native chat UI)

```
VSCode webview (chat UI inside an editor tab or sidebar pane)
   │  webview.postMessage(...)
   ▼
VSCode extension host (Node process, anthropic.claude-code-vscode)
   │
   │  child_process.spawn('/usr/local/bin/claude',
   │      ['--output-format','stream-json','--verbose',
   │       '--input-format','stream-json','--permission-mode',...],
   │      { stdio: ['pipe','pipe','pipe'], env: {...,
   │        CLAUDE_CODE_SSE_PORT='<random-port>', ...} })
   │
   │     ↳ Node 'pipe' is AF_UNIX SOCK_STREAM, not anon pipes — empirically
   │       confirmed via ls -la /proc/PID/fd/ showing `socket:[N]` for fd 0/1/2
   │
   ▼
claude agent (fd 0/1/2 → anon AF_UNIX SOCK_STREAM, CLAUDE_CODE_SSE_PORT set)
   │
   │  MCP client → 127.0.0.1:<sse-port> (extension's IDE MCP server,
   │  for getDiagnostics / executeCode / open-diff / selection-changed)
   │
   ▼  No tmux session anywhere.
```

Key properties:

- There is no tmux pane to capture and no tty to inject keystrokes into.
- claude-watch's daemon, even if it knew about this agent, has no
  mechanism to read what the agent is showing the operator — there's no
  pane scrape surface.
- The extension is also the lifecycle manager — it spawned the agent and
  holds the only writable end of the agent's stdin socketpair. Killing
  the extension host (closing VSCode) kills the agent.
- `inject_probe::inject` (PR #214, merged) uses `pidfd_getfd(2)` to dup
  the extension host's matching stdin-socketpair end and write a
  stream-json `user` message to it. This is now wired into the daemon
  dispatcher (PR #215). It is NOT a replacement for tmux orchestration,
  because:
  - It does NOT carry Escape — the agent processes the injected message
    on its next event-loop tick rather than cancelling in-flight
    generation.
  - It does NOT let claude-watch SEE what the agent is doing. Without a
    pane scrape surface, the activity-detection, token-stall, fresh-clear,
    foreground-block, and zombie-recovery features all stay dark.

### 2.3 What "the panel" means in Andrew's DMs

Critically, when Andrew says "I can Ctrl-C the panel-launched claude code
then execute it again (or run cw which is what i've been doing lately),"
the panel he is referring to is **VSCode's integrated terminal panel**
(the shell-prompt panel at the bottom of the editor), NOT the chat
webview from § 2.2. He runs `cw` in that terminal panel; `cw` is the
host-side shim documented in `examples/compose/bin/cw` that does
`docker compose exec -it claude-container tmux -u new-session -A -s
claude-container`. The result is § 2.1, with the operator's VSCode
integrated terminal as the tty client. That's the daily-driver shape on
the cw container today.

So the gap Andrew is asking about is NOT "expose the tmux agent inside
the chat webview" (a much bigger lift) — it's "the agent already runs in
tmux + container, can I get the rich-UX webview surface ALSO pointing at
that same agent." This reframing changes the problem substantially, as
explored below.

## 3. Research findings

### 3.1 What persists when the operator Ctrl-C's a "panel-launched" claude

Two cases, depending on which "panel" we mean:

**Case A — integrated terminal panel (cw container shape, the daily driver):**

- The VSCode integrated terminal panel is just an xterm.js terminal
  emulator. It owns the pty it was opened against; processes spawned in
  that terminal are children of the operator's shell, which is a child
  of VSCode's pty host.
- Ctrl-C against `cw` (i.e. against the foreground `docker compose exec`)
  sends SIGINT to docker's exec wrapper. The wrapper exits; the pty
  detaches from the in-container tmux session. Inside the container,
  **nothing is killed**:
  - `tmux new-session -d -s claude-container ...` (PID 18 in the
    container snapshot above) keeps running.
  - The agent in window 0 pane 0 keeps running.
  - The in-container `claude-watch` daemon keeps running.
  - Watcher supervisor, cron, and any other entrypoint-spawned processes
    keep running.
- The operator's terminal panel is now sitting at a shell prompt. Re-running
  `cw` (or `claude` if they wanted to launch a fresh in-container agent
  outside tmux) re-attaches a new pty to the same container session.
  The tmux session is the persistence boundary — the surrounding pty and
  the surrounding docker exec wrapper are disposable.

This is exactly the "tmux + docker IS the daemonization" property Andrew
articulated. The orchestration tmux is already detached-by-default and
the operator's terminal is just a transient client.

**Case B — chat webview (extension native panel, not the daily driver):**

- The webview is a Chromium frame hosted by the VSCode extension. Closing
  the editor tab disposes the webview but the extension host keeps running.
- When the operator clicks the in-webview "Stop" button (the equivalent
  of Ctrl-C for the chat UI), the extension sends an abort signal to the
  agent subprocess (terminating in-flight generation but not the
  process). When the operator closes the tab, the extension keeps the
  agent alive in the background for a grace window so re-opening doesn't
  drop state; eventually it tears the agent down.
- There is no "re-run claude" command in the webview itself — the
  webview re-spawns the agent the next time the operator clicks "New
  conversation" or re-opens the panel. The extension host process is
  the persistence boundary here.

Andrew's DM is plainly describing Case A. The "exposing something to
vscode" he intuits is the integrated-terminal pty + the docker exec
wrapper holding the operator's editor tab to the in-container tmux session
across the Ctrl-C / re-run cycle — not an extension-host hook into the
webview.

### 3.2 What the extension does on re-run (empirical)

To make sure we understand the extension-side dynamics for Case B even
though it's not the daily driver, the prior PR #214 work left a live
`spawn_panel_agent.js` Node wrapper running on this host that exactly
mimics the extension's `child_process.spawn(claude, {stdio:'pipe'})`
pattern. The wrapper is the persistent parent process; the spawned agent
is its child. To simulate the "Ctrl-C the agent + re-run it" sequence, a
fresh wrapper (`/tmp/inject-probe-ctrlc/parent.js`) was added that listens
for SIGUSR1, kills the current agent on receipt, and spawns a new one.

**Snapshot 1 — fresh cycle, agent pid 3605543 alive:**

```
PARENT 3605391 fds (socketpair endpoints):
lrwx------ ... 21 -> socket:[2092498963]      <-- write end to agent stdin
lrwx------ ... 23 -> socket:[2092498965]      <-- read end from agent stdout
lrwx------ ... 25 -> socket:[2092498967]      <-- read end from agent stderr

AGENT 3605543 fds:
lrwx------ ... 0 -> socket:[2092498964]       <-- stdin  (parent fd 21 + 1)
lrwx------ ... 1 -> socket:[2092498966]       <-- stdout (parent fd 23 + 1)
lrwx------ ... 2 -> socket:[2092498968]       <-- stderr (parent fd 25 + 1)
```

The inode-pairing rule from PR #214 holds: each socketpair endpoint has
consecutive inode numbers, with the parent-side end being the lower of
the two.

**Snapshot 2 — after `kill -USR1 <parent>`:**

```
parent.log:
cycle=1 agent_pid=3605543 ts=1778975627227
SIGUSR1 received, killing agent cycle=1
cycle=2 agent_pid=3622149 ts=1778975646229

agent.exit:
cycle=1 code=0 sig=null at=1778975645888
```

The cycle-1 agent exited cleanly. The parent stayed at the same PID
(3605391) and spawned a fresh agent at PID 3622149.

**Snapshot 3 — fresh cycle, agent pid 3622149 alive:**

```
PARENT 3605391 fds (same pid, new socketpair endpoints):
lrwx------ ... 21 -> socket:[2092791940]      <-- inode CHANGED
lrwx------ ... 23 -> socket:[2092791942]      <-- inode CHANGED
lrwx------ ... 25 -> socket:[2092791944]      <-- inode CHANGED

AGENT 3622149 fds (new pid, new socketpair endpoints):
lrwx------ ... 0 -> socket:[2092791941]       <-- agent stdin, paired with parent fd 21
lrwx------ ... 1 -> socket:[2092791943]       <-- agent stdout
lrwx------ ... 2 -> socket:[2092791945]       <-- agent stderr
```

**Observations:**

1. The **parent Node process persists across the agent cycle**. Same PID,
   same heap, same extension state. This is what makes the webview UI
   feel persistent — the webview's IPC partner is the extension host, not
   the agent.
2. The **socketpair endpoints are fully replaced per agent spawn**.
   Old inodes (`2092498963/964/965/966/967/968`) are gone; new ones
   (`2092791940-2092791945`) take their place. Node closes the old
   parent-side fds when the child exits and allocates a fresh
   `socketpair()` for the next `child_process.spawn`.
3. The **parent's fd numbers are reused** (21/23/25 again). Node's spawn
   path doesn't pin specific fd numbers; the kernel reuses the lowest
   available unallocated fds.
4. There is no fd-inheritance trick that "donates" the operator's prior
   socketpair to the new agent. The webview UI persists because the
   extension host buffers messages — it re-emits the chat history into
   the new agent's stdin on respawn rather than because the agent
   inherited any I/O state.

This confirms the model in `docs/sse-protocol.md` § "Two passes" and
extends it: the **extension host's identity is the persistent thing**.
Webview ↔ extension host is what survives; agent is short-lived; bytes
flow through the extension host both directions.

### 3.3 Could a tmux-hosted agent be made to appear in an open VSCode panel?

Three sub-questions, each answered separately.

#### 3.3.1 Can the integrated terminal panel host a tmux-hosted agent?

**Yes, already today, on the cw container daily-driver path.** Concretely:

1. The operator opens VSCode's integrated terminal (Ctrl-\`).
2. They type `cw` (or have it as their integrated-terminal default
   profile). The shim does `docker compose exec -it claude-container
   tmux -u new-session -A -s claude-container`. The operator's
   integrated-terminal pty becomes a tmux client attached to the
   container's tmux session.
3. The agent inside that session is what they interact with. Ctrl-C in
   the terminal cancels the operator's `docker compose exec` (their
   tmux client), not the agent. The detached tmux session keeps the
   agent alive; re-running `cw` re-attaches.

This is the "tmux + docker IS the daemonization" shape. The operator's
editor surface is just a pty client; the orchestration tmux is the
persistence boundary; the agent is reachable from claude-watch because
fd 0 is a pty inside that tmux pane.

The trade-off vs the chat webview:

| Property | Integrated terminal + cw (today) | Chat webview (today) |
|---|---|---|
| Persists across operator-quit | Yes — tmux+docker | Yes — extension host |
| claude-watch can observe | Yes — pane scrape | No — no surface |
| claude-watch can interrupt | Yes — tmux send-keys | Partial — pidfd inject (no Escape) |
| Image paste from clipboard | Same-host (Mac iTerm2 / native VSCode terminal): yes (osascript). Container / SSH / remote-tmux: yes WITH the xclip shim + Mac-side launchd daemon shipped in this PR; bare container without the daemon: no. See § 3.7. | Yes — base64-wraps into stream-json |
| Rich diff rendering | TUI-rendered (claude does its own diff UI in terminal) | Native VSCode diff editor (webview → extension → workspace) |
| @-mention picker | TUI completion menu (no LSP integration) | Native VSCode quick-pick with workspace symbols |
| Click-to-open files | Limited (terminal hyperlinks if supported) | Native (extension uses workspace.openTextDocument) |
| Speech-to-text | No | Yes (extension wires Whisper) |
| Plan / proposed-diff buttons | No (TUI) | Yes (extension renders accept / reject buttons in editor) |

The integrated terminal path gives up roughly the right-hand column. Most
of that loss is irreducible — the webview features rely on the extension
host being the IPC partner of the agent (e.g. the agent calls
`open_diff` via its MCP-IDE client, the extension renders that into the
workspace because it owns the workspace surface). A tmux-hosted agent
talking only to its own tty can't ask the extension to render anything
in the editor.

#### 3.3.2 Can the extension be reconfigured to "attach" to an existing agent instead of spawning?

**No, not as the extension ships today.** The extension's spawn path is in
`extension.js` and looks roughly like:

```js
// In ClaudeExtensionImpl.spawnClaude():
let { pathToClaudeCodeExecutable, executableArgs, env, nodePath } = this.getClaudeBinary();
L.pathToClaudeCodeExecutable = q;
L.executableArgs = M;
L.env = w;
if (F) L.executable = F;
return (await x40({options: L})).query(z);
```

`x40` is the `@anthropic-ai/claude-agent-sdk` `query()` factory. Its
internals call `Cr` (the subprocess transport class):

```js
class Cr {
  options; process; processStdin; processStdout; ...
  spawnLocalProcess(z) {
    const { command, args, cwd, env, signal } = z;
    const O = Uo.spawn(command, args, {
      cwd, stdio: ["pipe","pipe", stderrMode], signal, env, windowsHide: true
    });
    return { stdin: O.stdin, stdout: O.stdout, ... };
  }
  initialize() { ...this.spawnLocalProcess(...) }
}
```

There is **no "attach to existing process" branch** in the SDK transport.
The configuration surface (extension settings, env vars) does not include
a "use these fds" or "connect to this socket" option. The SDK supports
several transports (`stdio | http | sse-ide | ws-ide`) but those are
agent-as-MCP-client transports for connecting to MCP servers, NOT for
hosting the user-input channel of the agent itself — see
[`sse-protocol.md`](sse-protocol.md) for the full protocol surface. The
`claudeCode.claudeProcessWrapper` setting lets the operator override the
executable path, but not the transport.

**Could a wrapper executable pretend to be claude while actually proxying
to an existing tmux-hosted claude?** In principle yes:

1. Operator sets `claudeCode.claudeProcessWrapper` to a shim binary.
2. Shim reads stream-json from its stdin (the extension's pipe), translates
   to keystrokes, types them via `tmux send-keys` into the container
   session pane.
3. Shim reads tmux capture-pane output, translates it back to stream-json,
   writes to its stdout (the extension's reverse pipe).

This is technically feasible but the translation surface is enormous:

- The TUI agent emits ANSI-coded terminal output, not stream-json events.
  Reverse-translating "TUI screen with a tool-call animation + a partial
  assistant message + a sidebar" into stream-json events
  (`{"type":"assistant", ...content...}`, `{"type":"tool_use", ...}`,
  etc.) is a parser the size of the entire agent UI.
- Latency would compound — every model token would cross
  tmux-capture → ANSI-parse → stream-json-encode → IPC to extension.
- File path / diff / open_diff signals from the agent go through the
  MCP-IDE client to the extension. The tmux-hosted agent's MCP-IDE
  connection is to whatever SSE port was set in its env at startup,
  which is the container's tmux-spawned env — NOT the operator's current
  extension instance. The agent's open-diff calls would go to a stale
  port (or no port at all). Rewiring requires either telling the agent
  to re-dial a new SSE port at runtime (no such CLI exists today) or
  proxying the MCP server too.

A shim that re-implements both the chat-IPC AND the MCP-IDE bridge is a
multi-month project that competes directly with the extension itself.
Recommended: don't.

#### 3.3.3 Could the tmux-hosted agent register with the extension via the SSE port?

This is the inverse question — instead of "extension attaches to running
agent", "running agent connects to extension". The agent's MCP-IDE client
is already this shape, but it connects to the extension to *call tools*
(getDiagnostics, executeCode, open-diff) — it doesn't expose user-input.
The extension is the MCP server; the agent is the client. There is no
extension-side endpoint that says "this is a chat input I should
render".

**Verdict:** the SSE port is for `agent → extension` requests, not the
reverse. Re-purposing it for user input would require shipping a new
protocol on both sides.

### 3.4 The pidfd-inject path (PR #214) — what it does and doesn't solve

Already-shipped:

- `inject_probe::inject` can write a stream-json `user` message into a
  panel-mode agent's stdin via `pidfd_getfd(2)`. The agent picks it up on
  its next event-loop tick.
- `inject_dispatch::inject_to_agent` (PR #215) routes interruption
  injects to either `tmux::inject_text` (terminal mode) or
  `inject_probe::inject` (panel mode), based on `agent_deployment_mode`.

Limits (still open):

- **No Escape, no cancellation.** The pidfd-injected user message is
  processed AFTER the in-flight model generation finishes. tmux-mode
  interrupts cancel mid-generation by sending Escape + typing; the
  webview's own "Stop" button is the only equivalent and there's no API
  for it.
- **No pane scrape.** Activity detection (Thinking / ToolRunning /
  Writing / Idle / ForegroundBash / ShellPrompt), fresh-/clear detection,
  token-stall watchdog, prolonged-thinking detection, zombie recovery —
  all these depend on `tmux capture-pane` and have no panel-mode
  analogue. The webview renders the agent's stream-json output and the
  extension host could in principle expose a "capture" surface, but it
  doesn't.
- **Linux + same-uid only.** `pidfd_getfd` requires `ptrace_scope=0` or
  CAP_SYS_PTRACE. Cross-uid / locked-down hosts fall through to the
  event-tier escalation in `inject_dispatch::emit_event(...)`.

The pidfd path is the right thing for the narrow case "the operator
chose webview mode and we still need to deliver a mid-turn user message
to that agent." It is NOT the answer to "give one agent both
orchestration and rich UX."

### 3.5 Patterns from other VSCode extensions

Surveyed for comparison:

- **VSCode's debug adapter protocol (DAP).** Extensions can "attach to
  running process" — e.g. `attach: { processId: 1234 }` for the Python
  debugger. This works because the debugger (debugpy, lldb-vscode) is
  designed as a DAP server that the extension dials. The agent process
  itself stays oblivious; the debugger lives next to it. Claude Code
  doesn't ship a DAP-equivalent "I am a long-running daemon, here's my
  port" surface.
- **VSCode's LSP "external server" pattern.** Some language servers can
  run out-of-process and the extension dials them via TCP / pipes. Same
  shape as DAP — the server is the protocol surface; the
  language-aware tool is colocated. Claude Code's MCP-IDE port is
  agent → IDE, not the reverse.
- **Jupyter remote-kernel.** The extension can attach to a kernel running
  on a remote host. The Jupyter protocol explicitly supports this; the
  kernel publishes its connection info to a file (`kernel-XXXX.json`)
  that the extension reads. The closest analogue Claude Code has is
  `~/.claude/ide/<port>.lock`, but that file goes the wrong direction
  (it's the IDE-side MCP server, not an agent-input endpoint).
- **`code-server` / Remote SSH.** VSCode itself can split into a thin
  client (browser / native) talking to a remote server that hosts the
  extension. The agent then runs on the server side. This DOES let you
  separate "where the UI renders" from "where the agent lives" — but
  the UI is the whole VSCode UI, not just one extension's panel, and
  the server is a server install, not arbitrary tmux session.

The pattern that's missing for Claude Code is an "external agent"
transport in the SDK — a way for the extension to say "an agent already
exists, here's a handle to its IPC, plumb it as if you'd just spawned
it." Without that, the only handle the extension has is its own spawn.

### 3.6 The TUI agent vs the webview agent — are they "the same agent"?

A subtle finding from § 3.3.1's trade-off table: in terminal mode the
TUI is rendered by the claude binary itself (think `htop` / `vim` —
ncurses-style direct terminal painting). In webview mode the SAME claude
binary emits stream-json (`{"type":"assistant", ...}` events) which the
extension parses and renders with rich React components. They're the
same agent, but the rendering layer is different and the rendering
layer's features (the editor-integrated diff UI, the workspace
@-mention picker, paste-as-image base64-wrap) are extension-side, not
agent-side. You can't run the agent in TUI mode and still get the
extension-side rendering features; the agent isn't emitting the events
they need.

This is why "one agent process, both surfaces" is so hard. The choice of
transport at agent startup (`--input-format text` (TUI) vs
`--input-format stream-json` (webview)) is also the choice of which UX
features the operator gets. There's no `--input-format both`.

### 3.7 Image paste in TUI mode — empirical mechanism (2026-05-17)

The 2026-05-16 design doc claimed (line 320 of the original revision)
"Yes — VSCode terminal forwards OSC; tmux passes through (PR #173)" for
image paste in the cw container shape. **This is wrong.** Empirical
investigation (q-2026-05-17-a70e) found the actual code path:

**Where the bytes come from.** The bundled `claude` binary
(`@anthropic-ai/claude-code-linux-x64/claude`, v2.1.143) implements
TUI-mode clipboard image-paste as a subprocess invocation against the
host's native clipboard utility. Reverse-engineering the bundled JS:

```js
// Linux branch (paraphrased; same shape for darwin / win32)
{
  checkImage:  `xclip -selection clipboard -t TARGETS -o 2>/dev/null \
                | grep -E "image/(png|jpeg|jpg|gif|webp|bmp)" \
                || wl-paste -l 2>/dev/null | grep -E "image/..."`,
  saveImage:   `xclip -selection clipboard -t image/png -o \
                > $tmppath 2>/dev/null \
                || wl-paste --type image/png > $tmppath \
                || xclip -selection clipboard -t image/bmp -o > $tmppath \
                || wl-paste --type image/bmp > $tmppath`,
  // ... + osascript on darwin, powershell.exe on win32 / WSL
}
```

The binary then:

1. Spawns `xclip -selection clipboard -t TARGETS -o` (or `wl-paste -l`)
   to enumerate clipboard MIME types.
2. If an `image/*` MIME is present, spawns `xclip -selection clipboard
   -t image/png -o > /tmp/.../claude_cli_latest_screenshot.png`.
3. Reads the saved PNG bytes back, base64-encodes, attaches to the
   prompt as an image content block.
4. Emits a `tengu_paste_image` telemetry event with `input_image_paste`
   or `input_image_drag` as the sub-kind.

There is **no** OSC sequence, **no** SSE port use, **no** MCP-IDE call,
and **no** stdin-side image data. The TUI binary always reads the
clipboard through a subprocess against the host's display server.

**Why it works on Mac in iTerm2 / native VSCode integrated terminal.**
Both `claude` and the operator's clipboard live on the same Mac. The
`darwin` branch invokes `osascript -e 'the clipboard as «class PNGf»'`
which talks to the Mac WindowServer directly. Same host, same display
server, the clipboard is reachable. Works.

**Why it doesn't work in the cw container shape OUT OF THE BOX.** The
cw container has no display server (no `DISPLAY`, no
`WAYLAND_DISPLAY`, no Mac WindowServer). `xclip` / `wl-paste` either
are missing entirely or return an empty TARGETS list (the container's
`xclip -selection clipboard -t TARGETS -o` would fail with "Can't open
display"). The host operating the container also has no path to
forward its clipboard into the container — Docker's stdin / tmux / OSC
sequences don't carry clipboard data. The 2026-05-17 follow-up POC
ships an **xclip shim + file-watch bridge** (see Mitigations below)
that closes this gap end-to-end for operators willing to install a
small Mac-side launchd daemon; without that daemon, image paste in
TUI mode remains a no-op. Empirically verified on gomorrah
(2026-05-17):

```
$ docker exec compose-claude-container-1 bash -c \
    'which xclip; echo "DISPLAY=$DISPLAY"; echo "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"'
                                  # ← no xclip in container
DISPLAY=
WAYLAND_DISPLAY=
```

Even with `xclip` installed inside the container, there's nothing for it
to talk to. Confirmation also on the host: `gomorrah` itself has
`/usr/bin/xclip` but `DISPLAY=` is empty in shells reached via SSH from
a remote VSCode integrated terminal. Image paste in a remote-SSH +
terminal-mode claude on gomorrah doesn't work either, for the same
reason.

**Why env-var propagation (`CLAUDE_CODE_SSE_PORT`, etc.) does NOT fix
it.** The hypothesis going in was "the VSCode extension hosts an MCP
server, the integrated terminal sets `CLAUDE_CODE_SSE_PORT`, claude
auto-connects via `/ide`, and image-paste flows over that channel."
Code inspection of the binary shows this is not the case:

- The IDE MCP server hosts only `mcp__ide__getDiagnostics` and
  `mcp__ide__executeCode` as model-visible tools, plus a dozen
  CLI-internal RPCs for diff / selection / save. None of them is an
  image-paste delivery channel.
- `CLAUDE_CODE_SSE_PORT` is only consumed by the auto-connect path
  (`process.env.CLAUDE_CODE_SSE_PORT, K=q?parseInt(q):null`) to
  validate IDE lockfile entries against the current workspace. Setting
  it without a reachable corresponding lockfile under `~/.claude/ide/`
  on the right host accomplishes nothing — the lockfile validation
  also checks `process.ppid === lockfile.pid || lockfile.pid in
  ancestors(claude)`, which fails inside a container because the
  extension host isn't a process ancestor of the in-container claude.
- The clipboard read path quoted above ignores
  `CLAUDE_CODE_SSE_PORT` entirely and always shells out to
  xclip / wl-paste / osascript.

**The actual upstream gap.** This is also the feature request in
anthropics/claude-code#51244 — bridge the host clipboard image data
into the in-container CLI via the VSCode IPC socket
(`VSCODE_IPC_HOOK_CLI`, the same channel VSCode already uses to let
`code` invocations inside Remote SSH / devcontainers open files in the
host editor). Until that lands upstream there is no zero-config fix;
the shim shipped in this PR is a configuration-required workaround
that closes the gap for operators who set up the Mac-side launchd
daemon.

**Mitigations considered and where each lands.** The 2026-05-17 push-
back (Andrew: "i think we should at least be able to shim xclip")
forced an empirical re-investigation of each option below. Result:
**the xclip-shim path DOES work** when paired with an operator-side
clipboard daemon. Detail per option:

- **`xclip` shim backed by a file-watch bridge** — IMPLEMENTED
  (q-2026-05-17-a0ff). The shim at `container/bin/xclip` (installed
  at `/usr/local/bin/xclip` in the image, beating `/usr/bin/xclip` in
  PATH order) interprets the canonical claude argv shapes
  (`-selection clipboard -t TARGETS -o`, `-selection clipboard -t
  image/png -o`) and reads PNG bytes from a bind-mounted host
  directory (`/host-clipboard/` by default; override via
  `XCLIP_BRIDGE_DIR`). The operator-side daemon at
  `examples/compose/bin/clipboard-bridge-daemon` (Mac launchd agent;
  example plist at
  `examples/compose/launchd/org.gbre.claude-watch.clipboard-bridge.plist.example`)
  polls the Mac clipboard via `osascript` (preferred: `pngpaste` if
  installed), sha256-de-dupes, atomic-renames into a local bridge
  dir, and rsync's to the remote host on every clipboard change. The
  remote host bind-mounts that directory into the container at
  `/host-clipboard/:ro`. xclip-shim self-tests (11 cases) pass; the
  integration test (`container/tests/xclip-shim.test`) is wired into
  `make test-entrypoint`. The shim is graceful when the bridge dir
  is absent (returns "empty clipboard" rather than crashing), so a
  stripped-down `docker run` without the bridge is unaffected.

  Operator one-time setup steps (Mac side):
  1. Install rsync / pngpaste optionally:
     `brew install pngpaste` (recommended; faster + more reliable
     than the AppleScript fallback).
  2. Copy the launchd plist example, edit
     `CLIPBOARD_BRIDGE_REMOTE_HOST` / `CLIPBOARD_BRIDGE_REMOTE_DIR`,
     `launchctl load` it.
  3. On the remote host, ensure the bridge dir exists
     (`mkdir -p ~/.cache/claude-clipboard-bridge`).
  4. In `docker-compose.override.yml`, uncomment the
     `${HOME}/.cache/claude-clipboard-bridge:/host-clipboard:ro`
     bind-mount (see `docker-compose.override.yml.example`).
  5. `docker compose up -d --force-recreate claude-container`.

  Trust / privacy notes: the daemon copies ANY clipboard image
  change while it's running. Operators who care should
  `launchctl unload` the agent when not in a claude session, or
  restrict the rsync target with an SSH key whose `authorized_keys`
  forced-command pins it to `rsync --server -e.LsfxC ...` against a
  single directory.

- **`xclip` shim backed by SSH-to-Mac (direct callback)** — REJECTED
  for the Mac→gomorrah→container path. The shim would need to dial
  back to the Mac (the only host with the clipboard); that's a
  reverse direction across the SSH hop the operator initiated, which
  requires either a reverse-port-forward at SSH-time
  (`ssh -R 0.0.0.0:9999:localhost:22 gomorrah`, plus a Mac sshd, plus
  a pre-shared key) or a persistent control socket the daemon
  maintains. Strictly more moving parts than the file-watch bridge
  above for the same outcome. Re-evaluate if Andrew explicitly asks
  for the no-rsync-poll shape.

- **Native xclip via SSH X11 forwarding** — REJECTED for Andrew's
  layout. Theoretically: Mac runs XQuartz; `ssh -X hndrewaall@gomorrah`
  sets `DISPLAY=localhost:10.0` on gomorrah; docker exec passes
  `-e DISPLAY=$DISPLAY` into the container; in-container xclip dials
  gomorrah's localhost:6010 listener which sshd tunnels to Mac
  XQuartz; XQuartz pastes the Mac clipboard. In practice:
    1. **XQuartz is not pre-installed on macOS**; Andrew would need to
       install it. (Probe: `ls /Applications/Utilities/XQuartz.app`.)
    2. **VSCode's Remote-SSH integrated terminal does not enable X11
       forwarding by default**; operator would need a `RequestX11
       Forwarding yes` stanza in `~/.ssh/config` for the host AND a
       VSCode setting to pass through.
    3. **Container netns isolates localhost**: the in-container
       `xclip` can't reach gomorrah's `localhost:6010` listener
       without `--network=host` (which the compose stack does NOT
       use, for good reasons — port collisions, security).
       Workaround: `-e DISPLAY=host.docker.internal:10` on Linux only
       works with `--add-host=host.docker.internal:host-gateway` and
       a gomorrah-side sshd listening on the right interface. More
       knobs than the file-watch bridge for the same outcome.

  Empirically observed today: `DISPLAY=` is empty in shells reached
  via SSH from a remote VSCode terminal on Andrew's setup (the
  variable was never set on the gomorrah side), so the prerequisites
  are not in place. If a future operator DOES have XQuartz + VSCode
  X11 forwarding + a host-network container, the real
  `/usr/bin/xclip` (and `XCLIP_SHIM=disabled` to bypass the shim)
  would work — but for the dominant headless-Linux-host shape, the
  file-watch bridge is the supported path.

- **VSCODE_IPC_HOOK_CLI socket bridge** — REJECTED on empirical
  evidence. The IPC socket lives on the Mac at
  `/var/folders/.../vscode-ipc-XXX.sock`. VSCode Remote-SSH does NOT
  tunnel that socket back to the remote host; the `code` CLI inside
  a Remote-SSH terminal talks to the *remote* VSCode server (a
  different IPC endpoint synthesized on gomorrah). Probed via
  `code --help` inside the container — the `code` binary exposes
  `code --status`, `code --diff`, `code --add`, etc.; there is no
  clipboard read or write subcommand. The remote-side
  `VSCODE_IPC_HOOK_CLI` is reachable but has no clipboard API. Dead
  end without a VSCode extension running in the *Mac-side* extension
  host (Option D below).

- **OSC 52 clipboard read.** OSC 52 is `\033]52;c;<base64>\007` —
  used to *write* the terminal's clipboard. There is no widely-
  supported OSC for the inverse direction (read clipboard from
  terminal back to the process). xterm has a write-only `52;c;?`
  query that some terminals respond to, but it's rate-limited /
  disabled-by-default in many emulators (security: prevents pages
  from reading clipboard), and the claude binary doesn't implement
  reading it.

- **Wayland / X11 socket bind-mount on a headless remote host.** Not
  applicable: gomorrah has no display server. Local-Linux operators
  with a real display could `-v /tmp/.X11-unix:/tmp/.X11-unix
  -e DISPLAY=$DISPLAY` and use the real xclip, but that's a
  different deployment shape than the Mac-VSCode-Remote-SSH path
  Andrew uses.

**Conclusion (revised 2026-05-17).** The original Option A "image
paste works via tmux passthrough" claim is incorrect for any remote
setup. For the cw container shape on a headless host, the shipping
path forward is the **xclip shim + file-watch bridge** documented
above. The webview / upstream / save-to-file options below remain
valid alternatives operators can use without the launchd-daemon
prerequisite:

- **Shim path (this PR)** — install the launchd agent on the Mac,
  bind-mount `/host-clipboard` into the container, paste images
  directly in the TUI. ~250ms latency per paste (one rsync hop on a
  local LAN).
- **Option B** (webview sidecar) — paste images into the webview UI;
  the extension wraps them as base64 in stream-json and they reach
  the webview agent. Cross-poll via claude-events as documented in
  Appendix B. Useful when the operator doesn't want to install the
  launchd daemon.
- **Save-to-file** — drop the image in `~/scratch/` (or any
  bind-mounted host path), `@`-mention or reference the file path in
  the agent's prompt. Loses paste-and-go ergonomics but works without
  any infrastructure change.
- **Option D** (upstream VSCode IPC bridge) — track
  anthropics/claude-code#51244. Would obviate the launchd agent
  entirely. No homelab-side fix until it lands.

## 4. Design proposals

### 4.1 Option A — keep the cw container shape (status quo, working today)

- Operator runs the agent in the in-container tmux session.
- Operator's editor surface is VSCode's **integrated terminal panel**,
  attached via `cw`. They get TUI rendering + most VSCode-shell
  conveniences (terminal hyperlinks, image paste via tmux passthrough,
  ttyd / Remote SSH attach for parallel surfaces).
- claude-watch (in-container daemon) sees the pane, intervenes via
  tmux send-keys, all orchestration features work.
- Persistence: tmux + docker. Ctrl-C the integrated terminal → operator's
  exec dies → tmux session keeps running. Re-run `cw` → re-attach.

Pros:
- Already works. Zero code changes.
- Single source of truth for the agent. No state to reconcile across two
  surfaces.
- All orchestration features fire correctly.
- Same pattern works in ttyd (browser console), iTerm2 (native attach),
  ssh, any pty surface.

Cons:
- Operator gives up the webview-only features: native diff editor for
  proposed changes, native @-mention picker with workspace symbols,
  click-to-open files from chat history, speech-to-text, accept /
  reject buttons in the editor toolbar.
- Image paste in the cw container shape requires the **xclip shim +
  file-watch bridge** documented in § 3.7. The TUI clipboard read path
  is a subprocess invocation of `xclip` / `wl-paste` / `osascript`;
  inside a Linux container there is no display server, but the shim
  (baked at `/usr/local/bin/xclip` in the image) reads PNG bytes from
  a bind-mounted `/host-clipboard/` directory that a Mac-side launchd
  daemon populates via rsync on every clipboard change. Without the
  daemon installed, image paste no-ops gracefully (the shim treats an
  empty bridge dir as "no clipboard"). The remaining upstream gap is
  tracked at anthropics/claude-code#51244 (request: bridge the host
  clipboard into the CLI via the VSCode IPC socket). PR #173's tmux
  `allow-passthrough on` is still correct — it covers OSC sequences
  that carry truecolor / hyperlinks / bracketed-paste-of-text — but no
  part of the clipboard-image read path goes through tmux. See § 3.7
  for the full empirical investigation + mitigation matrix.

### 4.2 Option B — operator runs both, claude-event cross-pollination

- Operator runs the TUI agent in tmux (orchestration brain) AND opens
  the chat webview for ad-hoc rich-UX work (one-off image attachments,
  longer diff reviews).
- The two agents are separate Claude Code conversations. They share
  filesystem state (the workspace) but not chat history.
- Coordination via the existing event tier: a `claude-event` dropped by
  one shows up in the other's `UserPromptSubmit` context on its next
  turn. The webview agent can ping the tmux agent "I just landed a
  change to file X, please update your understanding" and vice versa.

Pros:
- Both surfaces get their best UX without compromise.
- No code changes — the event tier already exists.

Cons:
- Two agents = two billing meters, two context windows, double the
  catch-up time on cross-poll events.
- Operator has to decide where to type each message. Cognitively
  expensive.
- claude-watch only orchestrates the tmux one; the webview one is
  unsupervised by the daemon.

### 4.3 Option C — shim binary as `claudeProcessWrapper` (NOT recommended)

- Set `claudeCode.claudeProcessWrapper` to a shim that translates
  stream-json ↔ tmux send-keys + capture-pane.
- Webview talks to the shim, shim drives a tmux-hosted real claude,
  claude-watch sees the tmux agent.

Pros:
- One agent, both surfaces, in theory.

Cons:
- Translation surface is enormous (§ 3.3.2). Re-implements the entire
  agent UI parser.
- MCP-IDE re-wiring is its own subproject (the agent's MCP client dials
  whatever port was in its env at startup, not the current webview's
  port).
- Latency penalty per token.
- Maintenance burden grows with every release of claude — the agent's
  TUI output is not a stable parseable surface; it's painted directly to
  the terminal and only intended for human eyes.
- We'd be re-creating something equivalent to a webview-renderer
  ourselves, which is what the extension already is. Compete with the
  extension, lose.

### 4.4 Option D — upstream feature asks

Two separate upstream feature requests fall under this umbrella. They
target different limitations and can land independently.

**D.1 — agent transport "attach to existing"** (the one-agent-two-surfaces
ask):

- File an Anthropic-side feature request: add an SDK transport mode for
  "this agent is already running, here's how to plumb to it" (e.g.
  `attach: { stdinFd: 'unix:///path/to/sock', stdoutFd: '...' }` or
  similar).
- The extension would then have a way to use an existing agent instead
  of spawning a fresh one.

**D.2 — TUI clipboard bridge via VSCode IPC socket** (the image-paste-in-
container ask):

- Tracked upstream as anthropics/claude-code#51244 (open / stale as of
  2026-05-17).
- Request: when the in-container claude CLI detects
  `VSCODE_IPC_HOOK_CLI` in its env (set by VSCode's remote / devcontainer
  / Remote SSH bootstrap), have the clipboard-image-read path issue an
  IPC request to the VSCode host process over that socket instead of
  shelling out to `xclip` / `wl-paste` / `osascript`. The host-side
  VSCode would read its native clipboard and return the PNG bytes to
  the in-container CLI over the same socket.
- For the cw container shape specifically, also need `cw` to set
  `VSCODE_IPC_HOOK_CLI` in the `docker exec` env when the operator's
  integrated terminal has one, so it reaches the in-container claude
  process. This is the one piece we can ship locally — but it's only
  load-bearing once the upstream change lands; without the
  corresponding CLI codepath, propagating the env var alone
  accomplishes nothing (see § 3.7).

Pros:
- Clean architectural fix. Lets the extension be the rendering layer
  while leaving the agent process to whatever orchestration system the
  operator runs.

Cons:
- Long timeline (months to years to ship + adopt).
- Requires Anthropic to prioritize.
- Even with the transport, claude-watch still needs a pane-scrape
  equivalent — the webview's renderer doesn't expose its painted output
  in a parseable way.

## 5. Recommendation

**Adopt Option A as the canonical "one agent, full orchestration" shape
and document Option B as the supported "I need a temporary rich-UX
sidecar" shape. Do not pursue Option C. Track Option D as an
upstream-feature-request followup, low priority.**

Rationale:

- Option A works today, ships nothing new, and gives the operator the
  full orchestration surface (claude-watch interrupts, fresh-clear,
  resume-injection, zombie recovery, token-stall watchdog, prolonged-
  thinking interrupt, all of it). It's the daily-driver shape Andrew is
  already using on the cw container.
- The features Option A gives up vs the webview (editor-integrated diff,
  @-mention picker with symbols, speech-to-text, click-to-open, accept /
  reject buttons) are real UX losses but they're per-task quality of
  life, not core capability losses. The agent CAN do all those tasks in
  TUI mode; the operator just doesn't get the editor-rendered buttons.
- Option B covers the cases where the operator genuinely needs the
  webview features for a specific task (e.g. reviewing a 20-file diff
  with rich rendering, pasting a screenshot for context). Run the
  webview alongside the orchestration tmux; let them share the
  workspace + cross-poll via events; tear the webview down when the
  task is done.
- Option C (the shim) has too many failure surfaces and competes with
  the extension itself. The upside doesn't justify the engineering.
- Option D (upstream ask) is the right long-term fix but is months out
  at minimum and may never land. We can file the request and move on;
  shipping anything before that lands is premature.

### 5.1 Implementation plan (phased)

If we accept the recommendation, the actual implementation work is small:

**Phase 1 — documentation (this PR).** Land this doc. Update
`docs/sse-protocol.md` § "Implications for claude-watch" to point at
this doc for the "what should the operator actually do" question.

**Phase 2 — cw flow polish.** Make Option A more pleasant:

- `cw` already does the right thing on the container. Nothing to change.
- For native installs (no container), document the equivalent `tmux
  new-session -A -s claude` pattern so the operator gets the same
  shape locally. The `dashboard` script already creates such a session;
  point at it.
- VSCode-side: a one-liner for the operator's
  `terminal.integrated.profiles.linux` config that maps a profile name
  ("Claude container") to `cw`, so they get a one-click button in the
  integrated terminal selector. No code change required on our side.

**Phase 3 — event-tier polish for Option B.** Make cross-pollination
between a tmux-hosted agent and a transient webview agent smoother:

- Document the existing `claude-event` cross-poll pattern in this doc's
  appendix.
- If usage patterns show operators reaching for B frequently, consider a
  helper script that bootstraps a webview agent in the same workspace
  with a pre-seeded "I'm a sidecar, the orchestration agent is in tmux"
  system-prompt fragment. Not blocking on this for the initial doc.

**Phase 4 — upstream feature requests.** Two separate asks (see § 4.4):

- File the "attach-to-existing-agent transport" ask with Anthropic
  (D.1). No code on our side; github issue / support ticket. Track
  its status in this doc.
- Track / +1 anthropics/claude-code#51244 for the
  "TUI-mode-clipboard-bridge over VSCode IPC socket" ask (D.2). This
  is the prerequisite for image-paste in the cw container shape (see
  § 3.7 for the empirical investigation behind why it doesn't work
  today). When upstream lands, follow up with a small `cw` change to
  pass `-e VSCODE_IPC_HOOK_CLI=$VSCODE_IPC_HOOK_CLI` through `docker
  exec`. Until then, neither env-var propagation nor any local shim
  recovers image-paste in TUI mode against a remote / containerised
  host.

## 6. Open questions / unknowns

- **Webview pane-scrape equivalent.** Even if Option D ships (extension
  attaches to a running agent), claude-watch's pane-scrape features need
  a surface in webview mode. The extension renders stream-json events
  into the webview; could it also expose those events on a localhost
  port (or unix socket) for an observer like claude-watch? This is the
  bigger architectural question that the extension would need to answer
  for "full orchestration in webview mode" to ever work. Not blocking on
  Option A, but a known gap for any non-tmux deployment.

- **Activity-state semantics for webview agents.** Even with stream-json
  events available, the existing activity-state classifier
  (`detect_activity()`) is regex-driven against the TUI's painted output
  — keywords like "Thinking" / "Writing" / "Tool" appear in specific
  positions of the rendered frame. A new classifier driven off
  stream-json event types would be cleaner (`{"type":"assistant"}` =
  Writing, `{"type":"tool_use"}` = ToolRunning, etc.), but it's an
  entirely separate detector. Worth scoping if Option D ever lands.

- **Integrated-terminal-only host setups.** Some VSCode flavours (Cursor,
  VSCodium) disable or alter the chat-extension behavior. For Option A
  this is a non-issue (the integrated terminal works the same
  everywhere). For Option B we'd need to verify the chat webview is
  available in the operator's specific fork.

- **Image-paste in the cw container shape (and any remote setup).**
  The 2026-05-17 investigation (§ 3.7) found that TUI-mode image paste
  shells out to `xclip` / `wl-paste` / `osascript` against the local
  display server. In the cw container (and any SSH-from-Mac-to-headless
  -host setup) there's no display server reachable, so image-paste
  doesn't work. Closing this gap requires the upstream
  anthropics/claude-code#51244 (Option D.2) — there is no claude-watch
  side fix. PR #173's tmux `allow-passthrough` is still correct for the
  OSC sequences it actually covers (truecolor, hyperlinks, bracketed
  paste of text) but it is not load-bearing for clipboard images.
  Operator workarounds today: paste into a webview sidecar (Option B)
  or save-to-file and `@`-mention.

- **Where to default-recommend Option A vs Option B for new operators.**
  This doc recommends Option A as the canonical shape. The operator
  README + the `examples/compose/README.md` should converge on the same
  recommendation. Not part of this doc but a followup.

## 7. Out of scope

- **Re-implementing the chat webview against a tmux-hosted agent.**
  Option C (the shim). Explicitly rejected; § 4.3 details the failure
  modes.
- **Building a Claude Code "server mode" CLI flag.** Andrew explicitly
  ruled this out — "tmux + docker is enough for daemonization." We are
  not asking claude itself to host an input-listener; the orchestration
  layer is tmux.
- **Activity detection in webview mode.** Punted to Option D's landing
  point (§ 6). claude-watch operates against pane scrape today; webview
  mode requires a separate event-driven detector that doesn't exist yet
  and isn't being built in this design.
- **Bridging two agents' chat histories.** Option B accepts the operator
  has two separate conversations. Cross-poll via events is enough; we're
  not building a chat-history merger.
- **Replacing tmux as the daemonization layer.** Tmux + docker is the
  recommendation. Systemd-style supervision, Kubernetes operator,
  custom Rust daemon-of-daemons — all rejected. Tmux works, is
  well-understood, and the entire claude-watch tooling is built around
  it.

## Appendix A — quick reference: which mode am I in?

```
$ ls -la /proc/<claude-pid>/fd/0
```

| What you see | Mode | claude-watch behavior |
|---|---|---|
| `0 -> /dev/pts/N` | Terminal | tmux send-keys works. Full orchestration. |
| `0 -> socket:[N]` + `CLAUDE_CODE_SSE_PORT` in env | IdePanel | pidfd-inject works (no Escape). Limited orchestration. |
| `0 -> pipe:[N]` (anon pipe) | Unusual / CLI script | Falls through to Terminal default. |

Detection: `proc_util::agent_deployment_mode(pid)` returns
`Terminal | IdePanel | Unknown`. Dispatch: `inject_dispatch::inject_to_agent`
routes accordingly.

## Appendix B — claude-event cross-poll between two surfaces (Option B)

Operator has the tmux orchestration agent AND a webview sidecar open
against the same workspace.

```
# From the webview, drop a hint for the tmux orchestrator:
claude-event "user pasted screenshot of error X, archived to ~/scratch/err.png" \
    --tag webview-handoff --source manual

# The tmux agent picks this up in its next UserPromptSubmit context as:
#   EVENT[manual/webview-handoff]: user pasted screenshot of error X, ...
```

The reverse direction works identically — the tmux agent emits a
claude-event when it lands a change; the webview agent sees it on its
next turn.
