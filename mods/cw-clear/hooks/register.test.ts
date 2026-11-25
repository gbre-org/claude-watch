import { expect, test } from 'claude-code/testing'

// Mod mode is the default: with no options at all the mod registers /cw-clear
// and writes its heartbeat to the runtime-dir default self-clear reads.
test('on by default: registers /cw-clear and a default heartbeat path', async ($, on) => {
  const registered: string[] = []
  const writes: string[] = []
  on('command.register', (_$, e: any) => {
    registered.push(String(e.name ?? e.command ?? ''))
    return { value: { command: 'cw-clear' } }
  })
  on('env.get', () => ({ value: '/run/user/1000' }))
  on('fs.write', (_$, e: any) => {
    writes.push(String(e.path))
    return { value: {} }
  })
  on('clock.now', () => ({ value: 1000 }))
  on('clock.every', () => ({ value: undefined }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  expect(registered).toContain('cw-clear')
  expect(writes).toContain('/run/user/1000/claude-cw-clear-heartbeat.json')
})

test('enabled=false registers nothing', { options: { enabled: false } }, async ($, on) => {
  const registered: string[] = []
  on('command.register', (_$, e: any) => {
    registered.push(String(e.name ?? ''))
    return { value: { command: 'cw-clear' } }
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  expect(registered).toEqual([])
})
