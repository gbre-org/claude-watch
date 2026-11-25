import { expect, test } from 'claude-code/testing'

// The engine's own verdict for a Bash call it would open a dialog for.
const ASK = { decision: 'ask', reason: 'Dangerous rm operation on critical path: /x' } as const

const runDecider = (stdout: string, exitCode = 0) => () => ({
  value: { exitCode, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false },
})

test('an allow answer from the decider turns the engine ask into allow', async ($, on) => {
  let sent = ''
  on('process.run', (_$, e: any) => {
    if (e.argv?.includes('decide')) sent = String(e.init?.stdin ?? '')
    return runDecider('{"answer":"allow","rule_ids":["rm-file-in-roots"]}')()
  })
  on('tool.check', () => ASK)
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'rm -f /tmp/x.tmp' } })
  expect(r.decision).toBe('allow')
  expect(JSON.parse(sent).command).toBe('rm -f /tmp/x.tmp')
  expect(JSON.parse(sent).warning).toContain('Dangerous rm operation')
})

test('a deny answer becomes a deny whose reason is the note for the model', async ($, on) => {
  on('process.run', runDecider('{"answer":"deny","reason":"x","note":"rewrite with absolute paths"}'))
  on('tool.check', () => ASK)
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'cat <<EOF\nEOF' } })
  expect(r.decision).toBe('deny')
  expect(r.reason).toBe('rewrite with absolute paths')
})

test('abstain, a failing decider and bad output leave the engine verdict alone', async ($, on) => {
  let out = '{"answer":"abstain","reason":"dry-run"}'
  let code = 0
  on('process.run', () => runDecider(out, code)())
  on('tool.check', () => ASK)
  for (const [o, c] of [
    ['{"answer":"abstain","reason":"dry-run"}', 0],
    ['{"answer":"allow"}', 3],
    ['not json', 0],
    ['', 0],
  ] as const) {
    out = o
    code = c
    const r = await $.tool.check({ tool: 'Bash', input: { command: 'rm -f /tmp/x' } })
    expect(r.decision).toBe('ask')
  }
})

test('the decider is not consulted when the engine already allows', async ($, on) => {
  let calls = 0
  on('process.run', () => {
    calls++
    return runDecider('{"answer":"deny","note":"x"}')()
  })
  on('tool.check', () => ({ decision: 'allow' }))
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(r.decision).toBe('allow')
  expect(calls).toBe(0)
})

test('the decider is not consulted when the engine already denies', async ($, on) => {
  let calls = 0
  on('process.run', () => {
    calls++
    return runDecider('{"answer":"allow","rule_ids":["r"]}')()
  })
  on('tool.check', () => ({ decision: 'deny', reason: 'rule' }))
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(r.decision).toBe('deny')
  expect(calls).toBe(0)
})

test('enabled=false registers nothing', { options: { enabled: false } }, async ($, on) => {
  let calls = 0
  on('process.run', () => {
    calls++
    return runDecider('{"answer":"allow","rule_ids":["r"]}')()
  })
  on('tool.check', () => ASK)
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'rm -f /tmp/x' } })
  expect(r.decision).toBe('ask')
  expect(calls).toBe(0)
})

test('session.start writes a ready heartbeat and session.end a not-ready one', async ($, on) => {
  const beats: string[] = []
  on('process.run', (_$, e: any) => {
    if (e.argv?.includes('heartbeat')) beats.push(e.argv.join(' '))
    return runDecider('')()
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.end', () => ({ sessionId: 's1' }))
  on('clock.every', () => ({ value: undefined }))
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  expect(beats.some((b) => b.includes('--event session.start') && b.includes('--ready'))).toBe(true)
  await $.session.end({ reason: 'other' })
  expect(beats.some((b) => b.includes('--event session.end') && !b.includes('--ready'))).toBe(true)
})
