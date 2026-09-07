/**
 * Telegram EGRESS — the PRIVILEGED **send-initiation** capability (the sibling of `telegram-mcp.ts`).
 *
 * Where `telegram` is `reply{text}` — destination-LOCKED to the chat that woke the cycle, OPEN to any
 * tier — this server is `send_message{to, text}`: a PROACTIVE send to a destination the duck CHOOSES,
 * with no inbound message. That power is gated two ways:
 *   1. **min_trust: org** in the MCP registry — the harness only assembles this server for an org+
 *      cycle (operator `dack say`, a Telegram-org message). A `public` cycle (a stranger DM, a public
 *      group, a degraded boss cycle) never gets the tool — a prompt-injection can't make the duck
 *      spam the org group. The firebreak lives in Rust; this file is just the hands.
 *   2. **named destinations** — `to` is a symbolic name resolved against an operator-registered map
 *      (`TELEGRAM_DESTINATIONS`, static env from the gitignored config). The model can NEVER send to a
 *      raw/hallucinated/injected chat_id — only to a chat the operator pre-registered. Unknown name →
 *      fail-closed (the call returns the allowed names, sends nothing).
 *
 * Token: `TELEGRAM_BOT_TOKEN` (harness-injected, never the agent context). Raw Bot API `fetch`.
 * stdout is the MCP protocol channel — NEVER log to it; use stderr.
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { z } from 'zod'

// Operator-registered destinations: { "<name>": <chat_id> }. Static env from the gitignored config —
// NOT the soul — so the duck reaches only operator-known chats. Empty/missing ⇒ nothing is sendable.
function loadDestinations(): Record<string, number | string> {
  const raw = process.env.TELEGRAM_DESTINATIONS
  if (!raw) return {}
  try {
    const m = JSON.parse(raw)
    return m && typeof m === 'object' ? m : {}
  } catch (e) {
    console.error('[telegram-send] bad TELEGRAM_DESTINATIONS json:', e)
    return {}
  }
}

const DESTS = loadDestinations()
const NAMES = Object.keys(DESTS)

async function tgSend(body: Record<string, unknown>): Promise<unknown> {
  const token = process.env.TELEGRAM_BOT_TOKEN
  if (!token) return { ok: false, error: 'TELEGRAM_BOT_TOKEN not set' }
  const r = await fetch(`https://api.telegram.org/bot${token}/sendMessage`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  const j: any = await r.json().catch(() => ({}))
  if (!r.ok || j?.ok === false) return { ok: false, status: r.status, error: JSON.stringify(j).slice(0, 400) }
  console.error('[telegram-send] sent message_id=', j?.result?.message_id, 'to', j?.result?.chat?.id)
  return { ok: true, message_id: j?.result?.message_id, chat_id: j?.result?.chat?.id }
}

const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(v) }] })

const server = new McpServer({ name: 'telegram-send', version: '0.1.0' })

server.registerTool(
  'send_message',
  {
    description:
      `Proactively send a Telegram message (NOT a reply) to a known destination, ≤4096 chars. ` +
      `\`to\` must be one of the operator-registered destination names: ${NAMES.length ? NAMES.join(', ') : '(none configured)'}. ` +
      `You cannot send to a raw chat id — only these names. Use this to initiate (announce, ping); ` +
      `to answer a chat that messaged you, use the reply tool instead.`,
    inputSchema: { to: z.string().min(1), text: z.string().min(1).max(4096) },
  },
  async ({ to, text }: { to: string; text: string }) => {
    const chat = DESTS[to]
    if (chat === undefined) {
      return asText({ ok: false, error: `unknown destination "${to}"`, allowed: NAMES })
    }
    return asText(await tgSend({ chat_id: chat, text }))
  },
)

await server.connect(new StdioServerTransport())
console.error(`[telegram-send] ready; destinations: ${NAMES.join(', ') || '(none)'}`)
