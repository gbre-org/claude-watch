Restart the in-container Claude Code process to pick up a new binary version. This does NOT just clear context — it fully exits the inner `claude` process and respawns it inside the same tmux pane.

This is the container equivalent of the host's `/restart` skill. The host invokes `claude-watch update --force` against the systemd-managed daemon; the container invokes [`cwsr`](https://github.com/hndrewaall/claude-watch/blob/main/container/bin/cwsr), the in-container self-restart helper, which `npm install -g`s the new claude version and uses `tmux respawn-pane -k` to roll the inner process.

## Steps

1. **Trigger the in-container restart**: run `cwsr` (with no flags) inside the container. This:
   - Runs `npm install -g @anthropic-ai/claude-code@latest` against the named-volume-backed npm-global path (uid 1000 writable, no sudo needed).
   - Calls `tmux respawn-pane -k -t claude-container:0.0 <claude-cmd>` to kill the current claude process in pane 0 and start the freshly-installed version in its place. The argv is reconstructed from the same env vars `entrypoint.sh` used at container startup (`CLAUDE_SHIM_SETTINGS_PATH`, `CLAUDE_AUTO_CONTINUE`) so the shape survives the roll.

2. **Variant flags** (rarely needed):
   - `cwsr --version 2.1.150` — pin a specific npm version instead of `@latest`.
   - `cwsr --no-upgrade` — respawn the current claude process without an npm install (useful for picking up a config change that requires a process restart but not a new binary).
   - `cwsr --upgrade-only` — install the new version without rolling pane 0 (operator can `cwsr --no-upgrade` later to actually swap).
   - `cwsr --print` — dry-run; prints the planned npm + tmux argv and exits 0.

3. **Confirm**: tell the operator the in-container restart has been initiated. The current session's claude process will exit; the new one starts in the same pane. Tmux session, MCP bridges, named-volume claude versions/ dir, and the operator's `tmux attach` all survive the roll — only the inner `claude` process changes.

## When `/restart` (this skill) is NOT the right tool

- **Container itself is down**: the in-container `cwsr` binary obviously can't run if the container isn't up. Bring the container up with `docker compose up -d` (or `cw --up` from the host) — that path installs the freshest baked claude version automatically.
- **Need to change entrypoint-time env vars**: `CLAUDE_AUTO_CONTINUE`, `CLAUDE_CONTAINER_REWRITE_HOOKS`, `CLAUDE_HOST_PROJECT_DIR`, `CLAUDE_SHIM_PATTERNS`, etc. are baked at container startup. `cwsr` only rolls the inner process with whatever shape `entrypoint.sh` already chose. Ask the operator to `docker compose up -d --force-recreate` for those.
- **Want to clear context within the same binary**: that's `/clear`, not `/restart`. `/clear` is a Claude-Code-internal control-plane action; `cwsr` swaps the binary itself.

## Important

- `cwsr` is baked at `/usr/local/bin/cwsr`. Source: [container/bin/cwsr](https://github.com/hndrewaall/claude-watch/blob/main/container/bin/cwsr).
- The npm package name (`@anthropic-ai/claude-code`) and install command (`npm install -g`) are cross-platform — the same shape works whether the host is Linux, macOS, or Windows. Inside the container, npm runs as uid 1000 against a writable global path, so no `sudo` is needed.
- After the respawn, the new claude process loads the same managed-policy CLAUDE.md (`/etc/claude-code/CLAUDE.md`), the same bind-mounted `~/.claude/CLAUDE.md`, and the same project-tier CLAUDE.md, so the session-start checklist runs again.
