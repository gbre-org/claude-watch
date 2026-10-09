# Auto-resolving harmless permission prompts (`[autoresolve]`)

Claude Code raises a "Do you want to proceed?" dialog for Bash commands its
safety checks consider dangerous, even with bypass-permissions on. Some of
those are false positives that block an unattended sub-agent until somebody
answers. `[permission_prompt_monitor]` can only decline such a prompt after a
grace period. `[autoresolve]` can answer **Yes** to a narrow, explicitly
allow-listed class of them, immediately.

It is **default OFF**, audited, and **always decides**: a recognizable
permission dialog is never left hanging. A command matched by an allow rule
gets **Yes**; every other recognizable dialog (unknown command, hard-deny
class, parse failure, unrecognized dialog kind, unreadable rules) gets **No**.
There is no configuration that produces a default Yes.

## Preferred fix: avoid the prompt at the source

The prompt text says the check "cannot be auto-allowed by permission rules",
so no `permissions.allow` entry silences it, and there is no setting that
turns off the critical-path `rm` check (the `CLAUDE_CODE_DISABLE_*_RM_PROMPT`
switches in current builds cover only the command-substitution and inline
`sh -c` variants).

The false positive in the motivating case is a path-resolution quirk: the
check resolves a relative `rm` target against the session's working directory
rather than the directory the command `cd`'d into. `cd <worktree>/a/b/c &&
rm -f ../../../x.tmp` is evaluated as `<session cwd>/../../../x.tmp`, which
climbs to `/`, so it reads as `/x.tmp`. That is an inference from the shipped
bundle and the observed message; it has not been confirmed with the vendor.

Cheaper and safer than pane automation:

- have agents use **absolute literal paths** for `rm` (no `cd` + `../`), and
  put scratch files under `/tmp` or `mktemp -d` output spelled out literally;
- prefer `rm -f <abs path>` as its own command, not chained after `cd`.

Instruct agents accordingly (their prompts / project instructions). Use
`[autoresolve]` only as the fallback for what slips through.

## Modes and enablement

```toml
[autoresolve]
mode = "off"          # "off" (default) | "dry-run" | "enforce"; unknown = off
rules_file = "/abs/path/autoresolve-rules.toml"
max_per_minute = 6
trip_cooldown_secs = 900
settle_secs = 3
use_pane_cwd = false
default_answer = "no"        # only "no" is accepted; "yes" fails config load
undecided_alert_after = 6    # unreadable dialog-looking screen: alert after N cycles
no_after_secs = 20           # how long a dialog waits before a default-No is pressed
notify_agent_on_no = true    # also drop a note in the blocked agent's inbox
```

Rollout: copy `examples/autoresolve-rules.toml`, edit the roots, set
`mode = "dry-run"`, restart the daemon, and read the audit log for a few days.
Dry-run logs `would-answer` and `refused` lines and presses nothing. Move to
`"enforce"` only after the log shows exactly what you would have approved.

Kill switch (takes effect on the next cycle, no restart):

- `touch <state_dir>/autoresolve.disable`
- or `CLAUDE_WATCH_AUTORESOLVE_DISABLE=1` in the daemon's environment.

## What is answered

All of these must hold, otherwise nothing is pressed:

1. Strict dialog shape: title `Bash command` (optionally `from <agent>`), a
   command block, an optional `Dangerous rm operation on critical path:` line,
   the exact question `Do you want to proceed?`, exactly two options
   `1. Yes` / `2. No`, cursor on `1`, and no live prompt below it.
2. The same dialog, unchanged, for `settle_secs`, and again on a fresh capture
   immediately before the keystroke.
3. The **whole command** parses with a deliberately tiny tokenizer. Pipes,
   `||`, `&`, `$`, backticks, globs, `~`, heredocs, subshells and escapes are
   rejected. Wrapped lines are tried both ways (joined with and without a
   space); every reading must be allowed.
4. Every segment's program is covered by a rule, its flags are in that rule's
   `flags`, and every path (arguments, redirect targets, `cd` targets) is
   resolved against the command's own `cd` chain and lands inside a `[[root]]`
   (checked lexically and after symlink resolution, with a minimum depth so a
   worktree directory itself is never a target). A relative path with no
   known directory is refused. `rm` of a directory is refused.
5. No hard-deny term appears anywhere in the command: sudo, docker,
   kubectl/helm/terraform/cloud CLIs, network tools, host bridges, git push /
   history rewrites, `--force`, chmod/chown, and credential or secret paths
   (`.env`, `*.pem`, `.ssh`, `.aws`, `.netrc`, token/secret/credential names).
6. Rule `require_subagent = true` (default) means a main-loop dialog is never
   answered.

Pressed key: `Enter` on option 1 only. Never a digit.

## Rules file

`[[root]]` entries (`path`, `min_depth`) define where files may be touched.
`[[rule]]` entries (`id`, `programs`, `flags`, `answer = 1`,
`require_subagent`) enable programs from a fixed built-in list (`rm`, `cat`,
`touch`, `mkdir`, `echo`, `printf`, `sed`, `true`, `ls`, `cd`). The loader
rejects recursive flags, `answer` other than 1, programs outside the list and
overly broad roots. Missing, empty or unparsable rules mean nothing is answered.

## Audit and alerts

Append-only JSONL at `<state_dir>/autoresolve-audit.jsonl` (default: the
directory of `[general] state_file`). Each line has timestamp, pane, agent,
full command, warning line, resolved paths, rule ids, answer and mode; kinds
are `would-answer`, `answering`, `answered`, `answered-not-cleared`,
`aborted-screen-changed`, `refused`, `tripped`. If the audit line cannot be
written, no key is sent. Each answer (and each dry-run hit) is also emitted as
a low-severity claude-event (`autoresolve-answered`, `autoresolve-dry-run`).

More than `max_per_minute` answers trips the feature off for
`trip_cooldown_secs` and raises a high-severity `autoresolve-tripped` alert.
A dialog is never answered twice without the screen changing.

## Default No, undecided screens, agent notes

- **Recognizable dialog**: boxed title, a `Do you want ...?` question,
  consecutively numbered options with both an affirmative option and a bare
  `No`, exactly one cursor row, nothing live below it. Plan pickers and other
  menus do not qualify and are left alone.
- **Yes** needs the strict Bash shape plus a full rule match (all checks above).
  Anything else is **No**: the digit of the bare `No` option is pressed after
  `no_after_secs`; if the dialog survives, `Escape` is tried once. Audit kind
  `answered-no`, rule id `default-no`, with the reason and full command.
- **Variables**: commands containing `$VAR`, globs or substitutions (for
  example the "possibly-empty variable path" warning) are not expanded and
  therefore get No. Rewrite with absolute literal paths.
- **Undecided**: a screen that looks like a dialog but is partial or changing
  (box without a question, unreadable options, ambiguous cursor) causes no
  keystroke. After `undecided_alert_after` consecutive cycles a
  high-severity `autoresolve-undecided` alert is raised, and again every that
  many cycles while it persists.
- **Agent note**: after a No, if the dialog names an agent id (`a` + hex),
  `agent-msg send <id> <note>` delivers the reason and the command so the
  agent can rewrite it. With only an agent type label there is no id, so the
  command is carried in the audit event (`autoresolve-no`). Disable with
  `notify_agent_on_no = false`.
- In `dry-run`, No decisions are logged (`would-answer-no`) and emitted but
  nothing is pressed.

Kill switch and rate limit apply to every decision. With the feature off, the
old behavior (`[permission_prompt_monitor]` alert, then decline) is unchanged.

## Denial events (`autoresolve-no`)

Every automatic No raises a HIGH-priority claude-event tagged `autoresolve-no`
stamped `data.tier = "actionable"` so it routes to the main loop's actionable
list rather than ambient context. The message names the
denied agent (label and id when the dialog shows one), the dialog's own reason
line (for example "This shell -c script runs rm and could not be checked"),
the resolver's reason, and the full command. The main loop decides whether to
ask the operator for permission. Claude Code's removal guards are never
answered Yes unless an explicit allow rule matches the whole command.

An unrecognised permission-shaped dialog (question plus numbered options) that
stays unreadable past the grace period (`undecided_alert_after` cycles) is
declined with Escape and reported with the same event tag.
