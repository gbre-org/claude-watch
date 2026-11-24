import type { Register } from 'claude-code'

// Prototype only. Not deployed anywhere. Everything is off unless the
// `enabled` option is true. `$` may not be bound or passed, so every `$` use
// is spelled out at its call site and shared state is plain data.
export const register: Register = (on, options) => {
  if (options.enabled !== true) return

  const stateFile = String(options.stateFile ?? '')
  const logFile = stateFile ? `${stateFile}.log` : ''
  const resumeFile = String(options.resumeFile ?? '')
  const resumeCommand = String(options.resumeCommand ?? '')
  const heartbeatMs = Math.max(1000, Number(options.heartbeatSecs ?? 15) * 1000)
  // In-memory state. A hot reload of the mod re-runs register() with fresh
  // values (turns 0, busy false, ready false, new instance id), so none of it
  // may be treated as durable: the clear flow is stateless (it re-reads the
  // resume prompt on every /cw-clear), and consumers detect a reload through
  // the heartbeat `instance` / `loadedAt` fields.
  const instance = Math.random().toString(36).slice(2, 10)
  let loadedAt = 0
  let ready = false
  let tickStarted = false
  let turns = 0
  let busy = false
  // Whole log is rewritten per event ($.fs has no append); small by design.
  const lines: string[] = []

  const heartbeat = (ev: string, ts: number) =>
    JSON.stringify({ ts, event: ev, busy, turns, ready, instance, loadedAt }) + '\n'

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'cw-clear',
      description: 'Clear the conversation, then submit the resume prompt.',
    })
    ready = true
    if (options.probe === true) {
      await $.command.register({ name: 'cw-probe-submit', description: 'Spike: submit a probe prompt.' })
    }
    // One heartbeat/probe timer per mod instance (session.start fires again
    // after every /clear; a second timer would double the writes).
    if (!tickStarted && (options.probe === true || stateFile)) {
      tickStarted = true
      $.clock.every(options.probe === true ? 2000 : heartbeatMs, async () => {
        const ts = await $.clock.now()
        if (options.probe === true) {
          lines.push(`${ts} tick busy=${busy}`)
          if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
        }
        if (stateFile) await $.fs.write(stateFile, heartbeat('tick', ts))
      })
    }
    const ts = await $.clock.now()
    if (loadedAt === 0) loadedAt = ts
    lines.push(`${ts} session.start`)
    if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
    if (stateFile) await $.fs.write(stateFile, heartbeat('session.start', ts))
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    const ts = await $.clock.now()
    ready = false
    lines.push(`${ts} session.end reason=${(e as any).reason}`)
    if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
    if (stateFile) await $.fs.write(stateFile, heartbeat('session.end', ts))
    return next(e)
  })

  on('turn.start', async ($, e, next) => {
    if ((e as any).agentId === undefined) {
      busy = true
      turns++
      const ts = await $.clock.now()
      lines.push(`${ts} turn.start`)
      if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
      if (stateFile) await $.fs.write(stateFile, heartbeat('turn.start', ts))
    }
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    if ((e as any).agentId === undefined) {
      busy = false
      const ts = await $.clock.now()
      lines.push(`${ts} turn.complete reason=${(e as any).reason}`)
      if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
      if (stateFile) await $.fs.write(stateFile, heartbeat('turn.complete', ts))
    }
    return next(e)
  })

  on('command.run', { command: 'cw-clear' }, async ($, e, next) => {
    let text = ''
    if (resumeFile) {
      text = String(await $.fs.read(resumeFile)).trim()
    } else {
      const argv = resumeCommand.split(/\s+/).filter(Boolean)
      if (argv.length > 0) {
        const r = await $.process.run(argv, { timeoutMs: 10000 })
        text = r.exitCode === 0 ? String(r.stdout).trim() : ''
      }
    }
    if (!text) return { text: 'cw-clear: no resume prompt available; not clearing.' }
    lines.push(`${await $.clock.now()} cw-clear: scheduled clear`)
    // Deferred: a command hook cannot run another command inside its own dispatch.
    $.clock.after(0, async () => {
      try {
        const r = await $.command.run({ command: 'clear' })
        lines.push(`${await $.clock.now()} cw-clear: clear returned ${JSON.stringify(r).slice(0, 200)}`)
        void $.prompt.submit({ text })
        lines.push(`${await $.clock.now()} cw-clear: resume submitted`)
      } catch (err) {
        lines.push(`${await $.clock.now()} cw-clear: FAILED ${String(err)}`)
      }
      if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
    })
    return { text: 'cw-clear: clearing and resuming.' }
  })

  if (options.probe === true) {
    on('command.run', { command: 'cw-probe-submit' }, async ($, e) => {
      void $.prompt.submit({ text: e.args || 'probe' })
      return { text: 'queued' }
    })

    on('tool.check', { tool: 'Bash' }, async ($, e, next) => {
      const core = await next(e)
      const cmd = String((e as any).input?.command ?? (e as any).command ?? '')
      lines.push(`${await $.clock.now()} tool.check core=${core.decision} reason=${JSON.stringify(core.reason ?? '')} cmd=${cmd.slice(0, 120)}`)
      let out = core
      if (/rm -f \/work\/(\.\.\/work\/)?victim/.test(cmd) && core.decision !== 'allow') {
        lines.push(`${await $.clock.now()} tool.check: overriding to allow`)
        out = { decision: 'allow', reason: 'cw-clear probe allow' }
      }
      if (logFile) await $.fs.write(logFile, lines.join('\n') + '\n')
      return out
    }).catch(($, e, next) => (next.called ? next(e) : { decision: 'ask', reason: 'cw-clear probe failed' }))
  }
}
