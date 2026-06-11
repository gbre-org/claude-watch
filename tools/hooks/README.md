# hooks

PreToolUse / PostToolUse hook scripts that wire Claude Code's tool layer
into the obligations gate (`../obligations/`) and the session-task queue
(`../session-task/`). These are the canonical implementations — install
via `make install` from the repo root.

Each hook is a self-contained Python 3 script (no third-party deps). All
hooks default-open on any internal error: a broken hook must NEVER
blackhole the loop.

## Scripts

| Script | Hook event | Matcher | Purpose |
|--------|------------|---------|---------|
| `pre-agent-queue-gate-hook` | PreToolUse | `Agent` | Refuses Agent spawns missing `Queue item: q-XXXX` markers, or whose marker isn't `running`. |
| `pre-tool-obligations-gate-hook` | PreToolUse | `*` | Calls `obligations check`; denies when a gate-mode obligation's predicate is unsatisfied. Also enforces two hardcoded architectural gates: (a) bare-`watcher-ctl run` cardinal rule; (b) `Monitor` tool denied inside subagent context (`agent_id` non-empty) -- see [`docs/hooks.md`](../../docs/hooks.md#hardcoded-architectural-gates). |
| `pre-tool-claude-watch-alert-gate-hook` | PreToolUse | `*` | Denies every non-exempt tool call while any `[CLAUDE-WATCH]` alert is pending in `~/.config/claude-watch/pending-alerts.json`. Clear via `claude-watch-ack ack <id>` or `claude-watch-ack ack --all`. Exempt: the ack CLI itself, `session-task`, `git status/diff/log/commit/push/add`, `obligations list/show`, `self-clear`, and the `Read` tool. Companion `user-prompt-claude-watch-alert-record-hook` auto-records injected alerts. |
| `user-prompt-claude-watch-alert-record-hook` | UserPromptSubmit | `*` | Detects `[CLAUDE-WATCH]` injects in submitted prompts and records them as pending alerts via `claude-watch-ack add`. Silent no-op for ordinary prompts. |
| `pre-tool-dispatch-gate-hook` | PreToolUse | `*` | Counts consecutive non-exempt non-Agent tool calls; denies the (N+1)th once the threshold is crossed (default 6). Spawning an Agent (or `claude-watch-dispatch reset`) zeros the counter. Tweakable via `CLAUDE_WATCH_DISPATCH_THRESHOLD`, `CLAUDE_WATCH_DISPATCH_ENABLED=0`, `CLAUDE_WATCH_DISPATCH_BYPASS=1`, and `/etc/claude-code/dispatch-exempt.txt` (one regex per line). Exempt: `Read` + `Agent` tools and an inspection-only Bash list (`ls`, `pwd`, `cat`, `grep`, `rg`, `find` without `-delete`/`-exec`, `git status/diff/log/show/rev-parse/branch/blame`, `session-task`, `claude-watch-ack`, `claude-watch-dispatch`, `obligations list/show/status`, `self-clear`, `agent-msg inbox/list/status`). |
| `post-tool-obligations-update-hook` | PostToolUse | `*` | Runs `obligations post-tool` (satisfy-by-completion + inform-mode advisories), and manages the `no_pending_watcher_outputs` sidecar registry. |
| `post-tool-mark-attachment-read-hook` | PostToolUse | `Read` | Auto-marks external-messaging attachments as read via a host-specific `*-mark-read` shim when Claude opens a file under a configured attachment dir. Safe no-op when neither the shim nor the dir is present. |
| `pre-agent-background-required-hook` | PreToolUse | `Agent` | Denies `Agent` spawns missing `run_in_background: true`. Foreground agents block the main loop, defeating the dispatcher model. Env bypass `AGENT_FOREGROUND_OK=1` (audited) or per-prompt marker `FOREGROUND_AGENT_OK: <reason>`. |
| `pre-agent-worktree-isolation-hook` | PreToolUse | `Agent` | Denies `Agent` spawns whose prompt matches an opted-in shared-repo regex but lacks `isolation: "worktree"`. Prevents parallel agents on a single checkout from clobbering each other's branches during rebase / force-push. Opt-in via `/etc/claude-code/worktree-isolation-repos.txt` or `~/.config/claude-watch/worktree-isolation-repos.txt` (pipe-separated `repo_key\|prompt_regex\|upstream_path`). No-op when no config file is present. Also writes the Agent prompt to `~/.cache/claude/last-agent-prompt-<session>.txt` for the companion `worktree-create-hook`. Env bypass `WORKTREE_ISOLATION_BYPASS=1` (audited) or per-prompt marker `WORKTREE_ISOLATION_NOT_REQUIRED: <reason>`. |
| `worktree-create-hook` | WorktreeCreate / WorktreeRemove | n/a | Allocates a private `git worktree` under `/tmp/<repo_key>-worktrees/agent-<UUID>` for opted-in repos when the main session's cwd is not a git repo. Reads the same config files as `pre-agent-worktree-isolation-hook`. Tears down the worktree on `WorktreeRemove`. No-op when no config file is present or no opted-in repo matches the prompt. |

## Wiring example

In `~/.claude/settings.json` (after `make install` puts the binaries in
`~/bin/`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Agent",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/pre-agent-queue-gate-hook", "timeout": 5}
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/pre-tool-obligations-gate-hook", "timeout": 5}
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/post-tool-mark-attachment-read-hook", "timeout": 5}
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/post-tool-obligations-update-hook", "timeout": 5}
        ]
      }
    ]
  }
}
```

## Emergency bypass

  - `OBLIGATIONS_BYPASS=1` env var — pre-tool-obligations-gate-hook treats
    as allow + audits to `~/.config/claude/obligations-bypass.log`.
  - `QUEUE_GATE_BYPASS=1` env var — pre-agent-queue-gate-hook treats as
    allow + audits to `~/.config/claude/queue-gate-bypass.log`.
  - `CLAUDE_WATCH_ALERT_BYPASS=1` env var — pre-tool-claude-watch-alert-
    gate-hook treats as allow + audits to
    `~/.config/claude/claude-watch-alert-bypass.log`.
  - `CLAUDE_WATCH_DISPATCH_BYPASS=1` env var — pre-tool-dispatch-gate-hook
    treats as allow + audits to
    `~/.config/claude/claude-watch-dispatch-bypass.log`. Set
    `CLAUDE_WATCH_DISPATCH_ENABLED=0` to disable the dispatch gate entirely
    (allow + no state tracking).
  - `AGENT_FOREGROUND_OK=1` env var — pre-agent-background-required-hook
    treats as allow + audits to
    `~/.config/claude/agent-foreground-bypass.log`. Per-prompt marker:
    `FOREGROUND_AGENT_OK: <reason>` (same log).
  - `WORKTREE_ISOLATION_BYPASS=1` env var — pre-agent-worktree-isolation-hook
    treats as allow + audits to
    `~/.config/claude/worktree-isolation-bypass.log`. Per-prompt marker:
    `WORKTREE_ISOLATION_NOT_REQUIRED: <reason>` (same log).
  - `obligations override "<reason>" --duration <60|5m|1h>` — preferred
    audited bypass for the obligations gate; self-clears via TTL (24h cap).

## Tests

Two test scripts live under `tests/`:

  - `tests/pre-agent-queue-gate-hook.test` — exercises the queue gate
    against the real `session-task` CLI, using disjoint scopes from the
    live queue. ~8 cases.
  - `tests/pre-tool-obligations-gate-hook.test` — exercises the
    obligations gate (PreToolUse + PostToolUse) end-to-end against an
    isolated `HOME=$tmpdir` sandbox. ~140 cases covering every predicate
    (including `stale_ready_queue_present`), enforcement mode,
    exempt-patterns, overrides (including the pingme push-notification
    hook), the watcher-ctl cardinal-rule gate, and the `Monitor`
    subagent-block gate.

Run from the repo root with `make test-hooks`.
