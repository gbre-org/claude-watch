---
name: self-mcp-reconnect
description: "Force a stale MCP server's TOOL DISCOVERY to refresh — drive the /mcp picker to select a server and Reconnect via the baked self-mcp-reconnect tool; changes neither the binary nor the container"
---
Force a stale MCP server's tool discovery to refresh by driving Claude Code's `/mcp` picker — select the server, choose "Reconnect", and confirm it actually came back — via the baked `/usr/local/bin/self-mcp-reconnect` tool. This is for the case where `claude mcp list` already reports a server's transport as "Connected" but the SESSION's cached tool list (its `tools/list` result) is stale — most commonly `mcp-adaptor` after it was re-authenticated or restarted on the HOST side without the in-container session noticing.

**YES, this can be self-served.** `/mcp` is just an interactive TUI picker driven by `tmux send-keys`, the SAME input channel `self-clear` uses for `/clear` and `cwsr` uses to roll the binary. If `doc_search` (or another mcp-adaptor-backed) tool is missing mid-session even though `/mcp` shows the transport Connected, that's the signature this tool exists to fix — you don't need to ask the operator to attach and click through the picker by hand.

**This does NOT change the binary or the container.** It doesn't roll the inner `claude` process (that's `cwsr` / `/claude-container:claude-code-restart`) and doesn't recreate the container (that's `/claude-container:restart-container`). It only drives the already-running session's own `/mcp` UI to re-run one server's handshake.

## Why `claude mcp list` isn't enough

`claude mcp list` reports transport-level connectivity. A session's TOOL DISCOVERY is a separate, cached `tools/list` result fetched once at MCP handshake time. When a bridged server (typically `mcp-adaptor`, fronting `doc_search` / Slack / other corp tools) gets re-authenticated or bounced on the host, the transport can come back "Connected" while the session's cached tool list is still the pre-bounce one — so a tool the server now exposes (or exposes again) stays invisible until the session drives a fresh handshake. The only user-facing way to force that handshake is `/mcp` → select the server → "Reconnect".

## The `/mcp` picker is a real interactive modal

Confirmed empirically (2026-08-31, in a scratch tmux pane running a throwaway `claude` session — see the transcript notes in [container/bin/self-mcp-reconnect](https://github.com/hndrewaall/claude-watch/blob/main/container/bin/self-mcp-reconnect)'s module docstring): opening `/mcp` does **not** itself retry any connection. The full flow is:

```
/mcp
  -> "Manage MCP servers" list, one line per server:
       <name> · <status glyph> <status text>[ · N tools]
     cursor starts on the FIRST (alphabetically-sorted) server.
Down x N, Enter
  -> that server's detail screen: "1. <action>" / "2. <action>" / ...
     (a FAILED server's menu starts "1. Reconnect / 2. Disable"; a
     CONNECTED server's starts "1. View tools / 2. Clear authentication /
     3. Reconnect / 4. Disable" — the position of "Reconnect" varies by
     current status, so the tool locates it by TEXT, never a hardcoded index)
Down x M (to "Reconnect"), Enter
  -> fires the actual reconnect attempt. The menu closes on its own back to
     the normal chat prompt (no trailing Escape needed) and appends one of:
       "Reconnected to <name>."
       "Failed to reconnect to <name>: <error>"
```

So a correct reconnect needs real menu navigation with text-based lookups at each step — a blind `/mcp` + Enter (the older [`mcp-reconnect`](https://github.com/hndrewaall/claude-watch/blob/main/container/bin/mcp-reconnect) script's approach) only opens the list and leaves it sitting there; it never drives a server's own Reconnect action. `mcp-reconnect` still exists for callers that only need to force the list open (e.g. to eyeball transport status); `self-mcp-reconnect` is the tool for an actual, confirmed reconnect.

**Uncertainty flag:** the row-status glyphs, footer text, and action-menu wording above were captured from one live session on one Claude Code version. If a future Claude Code release changes the `/mcp` UI's copy or layout, `self-mcp-reconnect` fails LOUD (see exit codes below) rather than silently reporting success — treat a `menu-did-not-open` / `no Reconnect action` failure as "the UI shape moved, go re-verify in a scratch pane" rather than a transient error to retry blindly.

## Steps

1. **Confirm the symptom first.** `claude mcp list` shows the target server (usually `mcp-adaptor`) as Connected, but an expected tool from it (e.g. `doc_search`) is missing from the session's tool list. This is the case `self-mcp-reconnect` fixes — if the server is genuinely disconnected (not just stale-discovered), the same `/mcp` → Reconnect flow still applies, but `mcp-reconnect --verify` or a plain `/mcp` glance may already tell you enough without the full drive.

2. **Trigger the reconnect**: run `self-mcp-reconnect run` inside the container. Common invocations:
   - `self-mcp-reconnect run` — reconnect the default server (`mcp-adaptor`, or `$CLAUDE_SELF_MCP_RECONNECT_SERVER` if set). Backgrounds itself (forks) and returns immediately: `self-mcp-reconnect backgrounded (PID N) for server='mcp-adaptor'. Check \`self-mcp-reconnect status\` or the log for the result.`
   - `self-mcp-reconnect run --server <name>` — target a different MCP server by its exact `/mcp`-list name.
   - `self-mcp-reconnect run --foreground --json` — block and print one JSON result object on stdout. Use this for a PROGRAMMATIC caller (an agent, a cron producer) that wants the outcome synchronously instead of polling the state file. **From inside the very session being targeted, do NOT use `--foreground`** — the `/mcp` injection itself is non-cancelling (it never sends Escape, so it does not interrupt the pane's current turn), but it is still typed-and-submitted text, and Claude Code QUEUES a submission made while a turn is generating rather than running it immediately. Without the fork, the queued `/mcp` would deadlock behind the very turn running this command. Foreground mode is for an OUT-OF-SESSION caller (`docker compose exec`, a host script).
   - `self-mcp-reconnect status` — print the last run's result as JSON (`~/.local/state/claude-watch/self-mcp-reconnect.json` by default).

3. **Check the result.** Backgrounded runs write the outcome to the state file and log; poll `self-mcp-reconnect status` a few seconds after triggering, or tail the log path it prints. Exit codes (also present in the JSON `code` field):
   - `0` — reconnect confirmed: the pane showed `Reconnected to <name>.` for the target server.
   - `1` — usage error / no Claude Code pane found / internal error.
   - `4` — the server wasn't in the `/mcp` list, no "Reconnect" action was found on its detail screen, or a menu never rendered — a UI-shape mismatch, not a transient failure. Re-verify the picker's current layout in a scratch pane before retrying.
   - `5` — the reconnect was attempted and Claude Code itself reported failure (`Failed to reconnect to <name>: <error>`) — a real MCP-server-side problem (check the server / its auth on the host), not a scripting bug.
   - `6` — no result line appeared within the poll window (ambiguous — the tool sends a best-effort Escape to close any menu left open before exiting).

4. **Variant flags** (rarely needed):
   - `--menu-timeout N` — max seconds to wait for each `/mcp` screen to render (default 15).
   - `--result-timeout N` — max seconds to wait for the `Reconnected`/`Failed` result line after confirming Reconnect (default 20).
   - `--no-verify` — skip the secondary `claude mcp list` cross-check appended to the result.
   - `--log-file PATH` (env `$CLAUDE_SELF_MCP_RECONNECT_LOG`) / `--state-file PATH` (env `$CLAUDE_SELF_MCP_RECONNECT_STATE`) / `--lock-file PATH` (env `$CLAUDE_SELF_MCP_RECONNECT_LOCK`) — override the log / state / lockfile paths.

## When `/claude-container:self-mcp-reconnect` (this skill) is NOT the right tool

- **The transport itself is down** (not just stale tool discovery) and `/mcp` shows a genuine ✘ failed row with no prior successful auth: reconnect may still fix it, but if the underlying host-side bridge (e.g. `mcp-adaptor`) is actually unreachable, fix that first — `self-mcp-reconnect` can't repair a server that has nothing to reconnect TO.
- **You want to see the raw `/mcp` transport list, not drive a reconnect**: run bare `/mcp` yourself, or the older `mcp-reconnect` (no navigation, just opens the list — see the note above).
- **You need a NEW Claude Code binary**: that's `/claude-container:claude-code-restart` (`cwsr`); a binary roll re-runs MCP discovery from scratch anyway, so it's a heavier-handed alternative when a targeted reconnect doesn't help.
- **You need to re-run `entrypoint.sh` / pick up new bind-mounts or env vars**: that's `/claude-container:restart-container` or a full `make deploy-container` force-recreate.

## Important

- `self-mcp-reconnect` is baked at `/usr/local/bin/self-mcp-reconnect`. Source: [container/bin/self-mcp-reconnect](https://github.com/hndrewaall/claude-watch/blob/main/container/bin/self-mcp-reconnect).
- It IMPORTS `self-clear` as a sibling module for the shared tmux pane primitives (pane auto-discovery, FleetView focus-return, interrupt-and-wait, idle detection) instead of carrying a second copy — it must ship in the same directory as the baked `self-clear`, which `/usr/local/bin` is.
- It operates on the same Claude Code tmux pane `claude-container:0.0` that `self-clear` and `cwsr` target (auto-discovered the same way).
- A held lockfile means another `self-mcp-reconnect` run is already in progress; a new invocation no-ops rather than racing keystrokes with it.
- Menu navigation is driven purely by parsing captured pane text (server rows, numbered action rows, the final result line) — it never assumes a fixed cursor position or a fixed "Reconnect" slot number, since that slot's position depends on the server's current status.
