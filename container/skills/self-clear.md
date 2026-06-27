Trigger a clean context reset of the in-container Claude Code session — inject `/clear` followed by a resume prompt — via the baked `/usr/local/bin/self-clear` tool. This wipes the conversation context and bootstraps a fresh session that immediately picks up the resume prompt, WITHOUT waiting on the claude-watch daemon's own resume-injection path to fire.

**This clears CONTEXT only — it is NOT a restart.** The inner `claude` binary and the container both keep running; only the conversation context is reset (the equivalent of typing `/clear` yourself, plus an automatic resume prompt afterward). It does NOT roll the Claude Code binary (that's `/claude-container:claude-code-restart`) and does NOT recreate the container (that's `/claude-container:restart-container`). Use it for a PROGRAMMATIC context reset where you want the fresh session to continue a specific task immediately.

The tool injects keystrokes into the Claude Code tmux pane `claude-container:0.0`: it interrupts any in-flight thinking, sends `/clear`, polls tmux until the clear completes (token count drops and the prompt reappears), dismisses the post-`/clear` "How is Claude doing this session?" feedback prompt if present, then injects the resume prompt. It backgrounds itself (forks) so the call returns immediately while the clear+resume choreography runs to completion in the freshly-cleared pane.

## Steps

1. **Capture session-task state FIRST.** A context reset wipes everything not in the resume prompt. Before triggering self-clear, run `session-task set "<what to continue doing>"` with enough context for the fresh session to resume the current task / checklist correctly. Then make the resume prompt point at it.

2. **Trigger the reset**: run `self-clear` inside the container. Common invocations:
   - `self-clear` — clear context, then inject the built-in generic resume prompt (`[SELF-CLEAR-RESUME] Clean context reset completed. Resume the previous task / checklist.`). The fresh session re-runs the session-start checklist and continues from `session-task`.
   - `self-clear --resume-prompt "<text>"` — **STRONGLY RECOMMENDED**: pass a specific resume prompt that captures what the fresh session must do next (mirror / reference the `session-task set` state). The generic default is a fallback; a tailored prompt keeps the new session on-task instead of relying on a vague "resume the previous task" nudge.
   - `self-clear --no-resume` — inject `/clear` ONLY, no resume prompt (the fresh session is left at an empty prompt for the operator / daemon to drive).

3. **Variant flags** (rarely needed):
   - `--delay N` — max seconds to wait between `/clear` and the resume inject (default 15; the poll exits early once the clear is confirmed).
   - `--timeout N` — max seconds to wait for the pane to go idle before sending `/clear` (default 60).
   - `--log-file PATH` (env `$CLAUDE_SELF_CLEAR_LOG`) / `--lock-file PATH` (env `$CLAUDE_SELF_CLEAR_LOCK`) — override the log / lockfile paths. The resume prompt can also be set via `$CLAUDE_SELF_CLEAR_RESUME_PROMPT`.

4. **Confirm**: the command returns immediately (`Self-clear backgrounded (PID N)...`); the actual clear+resume runs in the background against pane 0. The current context is wiped a few seconds later and the fresh session starts with the resume prompt.

## When `/claude-container:self-clear` (this skill) is NOT the right tool

- **You just want a manual context clear, interactively**: that's the bare `/clear` control-plane action you type yourself. self-clear is the PROGRAMMATIC path — it adds the interrupt + poll-for-completion + auto-resume choreography on top, for when nobody is at the keyboard to type `/clear` and then a resume.
- **You need a NEW Claude Code binary / to pick up a new version**: that's `/claude-container:claude-code-restart` (backed by `cwsr`), which rolls the inner `claude` process. self-clear does NOT change the binary.
- **You need to re-run `entrypoint.sh` / re-seed obligations / pick up a rebuilt image / new bind-mounts / changed env vars**: that's `/claude-container:restart-container` (or a force-recreate, `make deploy-container`). self-clear touches NONE of that — it only resets the conversation context within the running binary.

## Important

- `self-clear` is baked at `/usr/local/bin/self-clear`. Source: [container/bin/self-clear](https://github.com/hndrewaall/claude-watch/blob/main/container/bin/self-clear).
- It operates on the Claude Code tmux pane `claude-container:0.0` (it auto-discovers the pane via `claude-watch status --json`, falling back to a direct tmux pane scan).
- Defaults are portable (XDG-based log / lock paths with `/var/...` fallbacks); a held lockfile means another self-clear is already running and the new invocation no-ops, so two resets can't race each other's keystrokes.
- Whatever the fresh session needs to continue MUST be in the resume prompt or in `session-task` — anything left only in the current context is gone after the clear.
