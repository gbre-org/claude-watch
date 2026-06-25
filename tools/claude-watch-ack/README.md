# claude-watch-ack

`claude-watch-ack` -- track and ack pending claude-watch alerts.

The companion `pre-tool-claude-watch-alert-gate-hook` (PreToolUse) DENIES
every non-exempt tool call while ANY alert is pending. This CLI is the
only mechanism (besides an audited bypass env-var) to clear the gate.

## Why this exists

claude-watch already injects banner prompts into the in-container tmux
session when it detects prolonged thinking, context-low, or watcher-down
states. The expectation -- per `baked-CLAUDE.md`'s `claude-watch alerts`
cardinal -- is that Claude STOPS EVERYTHING and attends the alert before
continuing.

In practice, soft prompt-layer guidance is unreliable. The v58 verdict on
this cardinal was "alert internalized: PARTIAL". After one acknowledgment
turn, the session resumes inline work without running the canonical
recovery steps (`session-task set`, commit, log).

This CLI + its companion hooks turn that soft expectation into a hard
tool-layer gate. Until the operator (or Claude) calls
`claude-watch-ack ack <id>`, every tool call is DENIED with a reminder
banner that points back to the canonical recovery flow.

## State file

`~/.config/claude-watch/pending-alerts.json` (0600).

```json
{
  "alerts": [
    {
      "id": "alert-20260515-143030-7421",
      "message": "[CLAUDE-WATCH] Prolonged thinking detected ...",
      "created_at": 1747400230,
      "source": "user-prompt-hook"
    }
  ]
}
```

Override the directory for tests via `CLAUDE_WATCH_ALERT_STATE_DIR`.

## Subcommands

```text
claude-watch-ack add <message> [--source TAG] [--json]
    Append a new pending alert. Prints the new id.

claude-watch-ack list [--json]
    Show pending alerts.

claude-watch-ack ack <id> [--confirm-read <token>]
    Remove a single alert by id. A bare `ack <id>` PRINTS the full body
    (which includes a per-alert `read-token=<hex>`) and REFUSES with exit 2 --
    it clears NOTHING. To clear, echo that token back:
    `ack <id> --confirm-read <token>`. The token is derived from the alert
    body and printed ONLY in the body, so supplying the right token is proof
    the body was read -- a blind/reflexive ack is structurally impossible.
    Exit 0 if ack'd, 1 if id not found, 2 if token missing/wrong.

claude-watch-ack ack --all --confirm-read
claude-watch-ack clear --confirm-read
    Remove every pending alert. A bare `--all`/`clear` PRINTS every body and
    REFUSES (exit 2); `--confirm-read` (after reading) clears them.

claude-watch-ack status [--json]
    Exit 0 when no alerts pending; exit 1 when any are.
```

## How alerts get recorded

The `user-prompt-claude-watch-alert-record-hook` (UserPromptSubmit)
detects the literal `[CLAUDE-WATCH]` substring in submitted prompts and
calls `claude-watch-ack add` automatically. claude-watch's
`tmux::inject_text(...)` injects the banner; once submitted, it becomes
a user prompt, the hook fires, and the alert lands in the state file
before the next tool call.

Manual invocation is supported too (`claude-watch-ack add "..." --source
manual`) for operator-side tooling.

## How the gate clears

The gate's PreToolUse hook reads `pending-alerts.json` on every tool
call. If any alert is pending, the hook DENIES the call with a banner
that:

  1. Lists the pending alert ids + first-line previews.
  2. Restates the canonical recovery (commit, log, `session-task set`,
     `self-clear`).
  3. Shows the exact `claude-watch-ack ack <id>` command to clear the
     gate.

Exempt patterns (allowed even while alerts pending):

  - `claude-watch-ack` (any subcommand).
  - `session-task` (compact-prep flow).
  - `git status` / `git diff` / `git log` / `git commit` / `git push` /
    `git add` / `git stash` / `git rev-parse` / `git branch` /
    `git checkout`.
  - `obligations list` / `obligations show` / `obligations status`.
  - `self-clear`.
  - The `Read` tool (file inspection).

Compound commands (`a && b`, `a; b`) pass the exempt check ONLY when
the first token matches an exempt pattern. `ls && claude-watch-ack ack
X` is denied; `claude-watch-ack ack X && ls` is allowed.

## Emergency bypass

`CLAUDE_WATCH_ALERT_BYPASS=1` in the environment makes the gate hook
default-open. Each bypass is audited to
`~/.config/claude/claude-watch-alert-bypass.log`. Use sparingly.

## Tests

`tests/claude-watch-ack.test` exercises every subcommand against an
isolated tmpdir via `CLAUDE_WATCH_ALERT_STATE_DIR`. Run from the repo
root with `make test-hooks`.
