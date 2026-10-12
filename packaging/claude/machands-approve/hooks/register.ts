import type { Register } from 'claude-code'

// Mac hands refuses a sketchy step (form submit, Send/Buy/Delete button, ...) with a 4-character code
// and the sentence a card would show. This mod turns that into a real Allow/Deny box: on Allow it
// unlocks the code with `winhands approve` (the Bash tool is denied that command, a mod is not) and
// reruns the same call with approval_code, so the model never handles the code itself.
// Typing "allow CODE" in chat still works through the UserPromptSubmit hook, for when the box can't show.

const TOOL = 'mcp__machands__computer'
const BIN = process.env.MACHANDS_BIN ?? `${process.env.HOME}/Applications/winhands.app/Contents/MacOS/winhands`
const CODE = /reply "allow ([A-HJ-NP-Z2-9]{4})"/
const SUMMARY = /"Mac hands wants to: ([\s\S]*?)"\. Ask them/

export const register: Register = on => {
  on('tool.call', async ($, e, next) => {
    const r = await next(e)
    if (String(e.tool) !== TOOL) return r
    const a = e as unknown as Record<string, unknown>
    if (a.approval_code) return r // a rerun: its answer stands
    const text = 'text' in r && typeof r.text === 'string' ? r.text : ''
    const code = text.match(CODE)?.[1]
    if (!code) return r
    const what = text.match(SUMMARY)?.[1] ?? 'run a step that needs your OK'

    let answer = ''
    try {
      answer = await $.ui.ask(`Mac hands wants to: ${what}. Allow it once?`, { options: ['Deny', 'Allow'], header: 'Mac hands' })
    } catch {
      return r // nobody to ask: the chat code in the refusal still works
    }
    if (answer === 'Allow') {
      const ok = await $.process.run([BIN, 'approve', code], { timeoutMs: 10_000 }).catch(() => undefined)
      if (!ok || ok.exitCode !== 0) return r
      return next({ ...e, approval_code: code } as typeof e)
    }
    await $.process.run([BIN, 'deny', code], { timeoutMs: 10_000 }).catch(() => undefined)
    return { deny: `The user chose Deny in the Mac hands box for: ${what}. It was not done. Do not retry it or look for another way.` }
  })
}
