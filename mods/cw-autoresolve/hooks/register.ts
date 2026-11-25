import type { Register } from 'claude-code'

// cw-autoresolve: answers Claude Code's Bash safety dialogs ("Dangerous rm
// operation ...", "can't be sure it read this command ...") at the
// tool-permission check, before any dialog is drawn.
//
// How: a `tool.check` hook on Bash. `next(e)` resolves to the engine's own
// verdict; only when that verdict is `ask` (the engine would open the dialog)
// does the mod consult `claude-watch autoresolve-mod decide`, which applies
// the same rules file, audit log, kill switch and rate limit as the tmux
// autoresolve path:
//   allow    -> { decision: 'allow' }  (allow-listed command: the "Yes")
//   deny     -> { decision: 'deny', reason }  (everything else: the "No"; the
//               reason is the tool error the model reads and rewrites from)
//   abstain  -> the engine's verdict stands (feature off, killed, tripped,
//               dry-run, decider failed): the dialog opens and claude-watch's
//               tmux path remains the fallback.
// The mod never turns an engine `allow`/`deny` into anything else.
//
// Liveness: a heartbeat is written through `claude-watch autoresolve-mod
// heartbeat` (so the file's path comes from claude-watch's config, one source
// of truth). The daemon trusts the mod only while the heartbeat is fresh.
//
// On by default. Set the `enabled` option to false to register nothing.
// `$` may not be bound or passed, so every `$` use is spelled at its call site.

type Answer = {
  answer?: string
  reason?: string
  note?: string
  rule_ids?: string[]
}

async function beat(
  $: any,
  bin: string,
  instance: string,
  event: string,
  ready: boolean,
): Promise<void> {
  try {
    await $.process.run(
      [bin, 'autoresolve-mod', 'heartbeat', '--instance', instance, '--event', event, ...(ready ? ['--ready'] : [])],
      { timeoutMs: 5000 },
    )
  } catch {
    // A missed beat only makes the daemon fall back to tmux.
  }
}

export const register: Register = (on, options) => {
  if (options.enabled === false) return

  const bin = String(options.claudeWatchBin || 'claude-watch')
  const heartbeatMs = Math.max(1000, Number(options.heartbeatSecs ?? 15) * 1000)
  const deciderTimeoutMs = Math.max(1000, Number(options.deciderTimeoutSecs ?? 8) * 1000)
  const instance = Math.random().toString(36).slice(2, 10)
  let tickStarted = false

  on('session.start', async ($, e, next) => {
    await beat($, bin, instance, 'session.start', true)
    if (!tickStarted) {
      tickStarted = true
      $.clock.every(heartbeatMs, async () => {
        await beat($, bin, instance, 'tick', true)
      })
    }
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    await beat($, bin, instance, 'session.end', false)
    return next(e)
  })

  on('tool.check', { tool: 'Bash' }, async ($, e, next) => {
    const core = await next(e)
    if (core.decision !== 'ask') return core
    const command = String((e.input as any)?.command ?? '')
    if (!command) return core
    let ans: Answer = {}
    try {
      const r = await $.process.run([bin, 'autoresolve-mod', 'decide'], {
        stdin: JSON.stringify({
          command,
          agent_id: e.agentId ?? null,
          warning: core.reason ?? null,
        }),
        timeoutMs: deciderTimeoutMs,
      })
      if (r.exitCode !== 0) return core
      ans = JSON.parse(String(r.stdout).trim().split('\n').pop() ?? '{}')
    } catch {
      return core
    }
    if (ans.answer === 'allow') {
      return { decision: 'allow', reason: `claude-watch autoresolve allowed (${(ans.rule_ids ?? []).join(', ')})` }
    }
    if (ans.answer === 'deny') {
      return { decision: 'deny', reason: String(ans.note || ans.reason || 'claude-watch autoresolve answered No') }
    }
    return core
  }).catch(($, e, next) => next(e)) // a failed hook leaves the engine's verdict (the dialog) and the tmux fallback
}
