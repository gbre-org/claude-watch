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
