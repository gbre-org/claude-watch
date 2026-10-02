# Tips & FAQ

Operational tips and common issues for claude-container users.

---

## Q: Image paste doesn't work from VSCode terminal into the container

When accessing the container via VSCode's integrated terminal (Docker attach or ttyd), Cmd+V / Ctrl+V doesn't trigger Claude Code's image paste.

**Root cause:** VSCode intercepts Ctrl+V as its own "paste" shortcut and never sends the raw `\x16` byte to the terminal. Claude Code's `chat:imagePaste` keybinding is bound to Ctrl+V but never receives it.

**Fix:** Add this VSCode keybinding (Cmd+Shift+P → "Open Keyboard Shortcuts (JSON)"):

```json
{
    "key": "cmd+v",
    "command": "workbench.action.terminal.sendSequence",
    "args": { "text": "" },
    "when": "terminalFocus"
}
```

This sends raw Ctrl+V (`\x16`) to the terminal when focused. Claude Code receives it and triggers image paste via the xclip shim.

**Note:** With this keybinding active, text paste in the terminal uses Ctrl+Shift+V or right-click paste instead of Cmd+V.

**Prerequisites:** The clipboard bridge must be running (Layer A: daemon polls Mac clipboard → Layer B: compose bind-mount → Layer C: xclip shim in container). See `examples/compose/bin/clipboard-bridge-daemon` and `examples/compose/launchd/` for setup.

## Q: The log says "prompt line holds text ... skipping this inject" and my update (or other inject) never lands

Before the daemon types into Claude Code's input box (auto-update's `/exit`,
interrupts, reminders) it checks the prompt row for text that is already
there, and refuses to type over it. Two different things can sit on that row:

- **Faint ghost text** — Claude Code's own prompt autosuggestion. It is drawn
  with the faint attribute (SGR 2, `\e[2m...\e[0m`) and is not anything a human
  typed; it regenerates from the conversation, so it can never be waited out.
  The guard recognises it and proceeds (log line: "holds only faint ghost
  text").
- **Ordinary-weight text** — a person mid-keystroke, or other UI chrome drawn
  on the prompt row (a picker row such as `❯ (current)`, a hint). The guard
  skips the inject and logs the text as `residue` plus `rendering`, the raw
  row with ESC shown as `\e`. Read `rendering` first: if the text is wrapped in
  `\e[2m`, it is a suggestion (and a bug in the faint detection); if it is
  bare, it really is plain text on the row.

Plain `tmux capture-pane -p` drops all rendering, so it cannot tell these
apart; use `tmux capture-pane -p -e` (read-only) to see the escapes. The guard
is deliberately conservative: a single non-faint character on the row makes
the whole row count as real input.
