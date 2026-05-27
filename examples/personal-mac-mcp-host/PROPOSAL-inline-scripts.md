# Proposal: running non-trivial inline scripts over the host MCP bridge

> **Status:** PROPOSAL — pick a direction before anyone implements. Nothing
> here is built yet.

## The problem

The host MCP bridge exposes the host through `cli-mcp-server`'s single
`run_command` tool. `cli-mcp-server` is an off-the-shelf PyPI package; the
launcher (`examples/compose/bin/mcp-host-bash`) just stands up
`mcp-proxy` → `cli-mcp-server` over HTTP and feeds it an env-driven
allow-list. `run_command` takes a command string and **shlex-tokenizes it**
before exec.

That tokenizer chokes on complex inline quoting **even with the most
permissive settings** (`ALLOWED_COMMANDS=all`, `ALLOW_SHELL_OPERATORS=true`):

- single-quoted bodies containing `qw(...)`, `;`, `(`, `)`
- nested quotes (`"..."` inside `'...'`)
- multi-line / heredoc payloads

…all fail with `Invalid command format: No closing quotation`.

### Real failing example

A remote agent wants to run a small Perl one-liner on the host:

```
perl -e 'my @x = qw(a b c); print "$_\n" for @x;'
```

`run_command` rejects it (`No closing quotation`) because the tokenizer can't
reconcile the embedded `qw(...)` and the `;`. The only workaround today is to
base64-encode the script on the sending side and decode it on the host:

```
echo <base64> | base64 -D | perl       # macOS uses -D, GNU uses -d
```

That is exactly the ceremony we want gone: the agent has to remember to
encode, remember the host's decode flag (`-D` vs `-d` differs by platform),
and the failure mode when it forgets is a confusing tokenizer error rather
than a real syntax error.

## North star

> "emphasize simplicity and minimizing cognitive overhead for agents."

An agent should hand over a multi-line script **as-is** — zero quoting or
encoding ceremony — and have it run.

## Candidate fixes

Scored on **impl effort** (how much we build/maintain) and **agent overhead**
(ceremony the agent must perform per call). Lower is better on both.

### Option A — host `run-script` helper (allowlisted binary)

A tiny script on the host (e.g. `run-script <interpreter>`) that reads the
script body from **stdin** (or a single argv slot) and `exec`s the chosen
interpreter on it. It is added to the allow-list, so the agent calls it via
the existing `run_command`:

```
run-script perl        # body piped on stdin
```

- **Impl effort: low.** ~30-line wrapper + one allow-list entry. No new MCP
  surface; pure off-the-shelf `cli-mcp-server`.
- **Agent overhead: medium.** Removes interpreter-flag/quoting knowledge, but
  the agent still goes *through* `run_command`, whose tokenizer still parses
  the `run-script perl` argv line. Getting the body onto stdin without shell
  ceremony depends on how the harness feeds stdin to `run_command` — if it
  can't, you're back to quoting/piping. Argv-slot variant re-introduces the
  same tokenizer that caused the problem.

### Option B — `run_script(interpreter, script)` MCP tool (RECOMMENDED)

Add a second MCP tool alongside `run_command`. It takes two **structured
string arguments** — `interpreter` and `script` — and pipes `script` to the
interpreter's **stdin**. The `script` argument is a normal JSON string in the
tool call; **no shell, no shlex, no tokenizer touches it at any layer.**

```jsonc
run_script({ "interpreter": "perl", "script": "my @x = qw(a b c);\nprint \"$_\\n\" for @x;" })
```

- **Impl effort: medium.** This is more than `cli-mcp-server` ships, so it
  needs a small custom stdio MCP server (or a thin shim that registers the
  extra tool and shells out to the allowlisted interpreter). ~100-150 lines of
  stdlib Python, plus tests + the same allow-list / `ALLOWED_DIR` / timeout
  fencing `run_command` already enforces. That custom surface is the real
  cost: we now own a tool instead of leaning entirely on the PyPI package.
- **Agent overhead: lowest.** The agent passes the script as a plain
  structured argument. Multi-line, quotes, `qw(...)`, `;`, heredocs — all just
  string bytes. There is nothing to encode, escape, or remember.

### Option C — document base64 as the official pattern (REJECT)

Bless the `echo <b64> | base64 -d | interp` workaround as the documented way.

- **Impl effort: ~zero** (docs only).
- **Agent overhead: high — and permanent.** This is precisely the ceremony
  the north star wants eliminated: encode every time, remember the
  platform-specific decode flag, and debug opaque failures when forgotten.
  Listed for completeness; **reject.**

### Scorecard

| Option | Impl effort | Agent overhead | Off-the-shelf? |
|---|---|---|---|
| A — `run-script` allowlisted binary | low | medium (still via tokenizer) | yes |
| **B — `run_script` MCP tool** | medium | **lowest** | no (custom tool) |
| C — base64 docs | none | high (permanent) | yes |

## Recommendation

**Option B — add a `run_script(interpreter, script)` MCP tool.** It is the
only option that takes the script *entirely out of any shell/tokenizer path*,
which is what makes the agent overhead genuinely zero. The cost is a small
custom MCP tool we maintain (vs. pure off-the-shelf), but that is a one-time,
well-bounded surface and it is the price of the north star. Option A's lower
impl cost is offset by the fact that the body still has to traverse the same
tokenizer that broke us; it only half-solves the problem.

### Change surface (sketch)

- A small stdio MCP server (or a shim fronting `cli-mcp-server`) that
  registers `run_script` in addition to `run_command`. ~100-150 lines of
  stdlib Python.
- Reuse the existing policy fences: resolve `interpreter` against the same
  allow-list, keep `ALLOWED_DIR` as cwd, apply the per-command timeout. The
  script body is **never** parsed — only written to the interpreter's stdin.
- Wire it into the launcher (`examples/compose/bin/mcp-host-bash`) behind the
  same `mcp-proxy` front and bearer-auth shim that already exist; no transport
  or auth changes.
- Embedded test(s) covering the failing example above (perl `qw(...)`, a
  multi-line bash heredoc, nested quotes) to lock in the no-tokenizer
  guarantee.

## Backward compatibility

**Additive, not breaking.** `run_command` stays exactly as-is for the simple
argv case agents already rely on. `run_script` is a *new* tool; nothing about
the existing tool, its allow-list semantics, or the bridge transport changes.
Operators who never call `run_script` see no behavior difference.
