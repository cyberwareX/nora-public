/**
 * Telegram SEND — the PRIVILEGED relay tool (sibling of the destination-locked `reply`).
 *
 * The reply tool reaches ONLY the chat that woke the cycle — correct for guests, but it means an
 * operator answering an escalation in THEIR chat could never reach the guest (the answer looped
 * back to the operator: the live bug this fixes). This server is `send_to_guest{chat_id, text}` —
 * a send to a chat the CYCLE chooses — gated two ways:
 *   1. `min_trust: org` in the registry — assembled only for operator-lane cycles; a guest cycle
 *      (public taint) never sees the tool, so a prompt-injection can't request a cross-chat send.
 *   2. **Known-chat allow-list, fail-closed** — the target must be a chat Nora already knows:
 *      a Sibyl `guest` entity (deep-link-bound chats), a `reservation` carrying that chat, or a
 *      chat that ESCALATED (source_chat in the escalation log). A hallucinated / arbitrary id —
 *      including negative group ids — is refused and sends nothing.
 * Every attempt is audited to sends.jsonl (ok or refused) — delivery ground truth.
 *
 * stdout is the MCP protocol channel — NEVER log to it; use stderr.
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { Database } from 'bun:sqlite'
import { appendFileSync, existsSync, readFileSync } from 'node:fs'
import { z } from 'zod'

const TOKEN = (process.env.TELEGRAM_BOT_TOKEN ?? '').trim()
const SIBYL_DB = process.env.SIBYL_MEMORY_DB ?? '../.sibyl-data/memory.db'
const ESCALATION_LOG = process.env.ESCALATION_LOG ?? '../escalations.jsonl'
const SENDS_LOG = process.env.SENDS_LOG ?? '../sends.jsonl'

/** Is this a chat Nora legitimately knows? Fail-closed: any error or miss → false. */
function isKnownGuestChat(chatId: string): { known: boolean; via?: string } {
  if (!/^\d{4,15}$/.test(chatId)) return { known: false } // positive user/private ids only
  try {
    const db = new Database(SIBYL_DB, { readonly: true })
    try {
      const g = db.query("SELECT 1 FROM entities WHERE category='guest' AND name=? LIMIT 1").get(chatId)
      if (g) return { known: true, via: 'guest-entity' }
      const r = db
        .query("SELECT 1 FROM entities WHERE category='reservation' AND body LIKE ? LIMIT 1")
        .get(`%${chatId}%`)
      if (r) return { known: true, via: 'reservation' }
    } finally {
      db.close()
    }
  } catch (e) {
    console.error('[telegram-send] sibyl check failed (fail-closed):', e)
  }
  try {
    if (existsSync(ESCALATION_LOG)) {
      for (const line of readFileSync(ESCALATION_LOG, 'utf8').split('\n')) {
        if (!line.trim()) continue
        try {
          if (String(JSON.parse(line).source_chat ?? '') === chatId) return { known: true, via: 'escalation' }
        } catch {
          /* skip */
        }
      }
    }
  } catch (e) {
    console.error('[telegram-send] escalation-log check failed (fail-closed):', e)
  }
  return { known: false }
}

function audit(entry: Record<string, unknown>): void {
  try {
    appendFileSync(SENDS_LOG, JSON.stringify({ at: Math.floor(Date.now() / 1000), ...entry }) + '\n')
  } catch (e) {
    console.error('[telegram-send] audit append failed:', e)
  }
}

const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(v) }] })

const server = new McpServer({ name: 'telegram-send', version: '0.1.0' })

server.registerTool(
  'send_to_guest',
  {
    description:
      'Deliver a message to a KNOWN guest chat — for relaying the owner\'s answer to an escalation, ' +
      'or reaching a bound guest outside their own wake. `chat_id` must be a chat Nora already ' +
      'knows (a bound guest, a reservation\'s chat, or the chat that escalated) — copy it from the ' +
      'escalation or the guest entity, NEVER guess: an unknown id is refused and nothing sends. ' +
      '≤4096 chars. After a successful send, confirm to the owner in their own chat via `reply`.',
    inputSchema: { chat_id: z.string().min(1).max(20), text: z.string().min(1).max(4096) },
  },
  async ({ chat_id, text }: { chat_id: string; text: string }) => {
    const target = chat_id.trim()
    // Idempotency: an identical (chat, text) that already DELIVERED within the window is not sent
    // again — return the prior success so a model "making sure" can't double-message a guest
    // (observed live: message_ids 22+23, 49s apart, same 254 chars).
    const DUP_WINDOW_SEC = 600
    try {
      if (existsSync(SENDS_LOG)) {
        const now = Math.floor(Date.now() / 1000)
        for (const line of readFileSync(SENDS_LOG, 'utf8').trim().split('\n').slice(-50)) {
          try {
            const p = JSON.parse(line)
            if (p.ok && p.chat_id === target && p.chars === text.length && now - p.at < DUP_WINDOW_SEC) {
              audit({ tool: 'send_to_guest', chat_id: target, ok: true, suppressed: 'duplicate', message_id: p.message_id, chars: text.length })
              console.error(`[telegram-send] duplicate suppressed (already delivered message_id=${p.message_id})`)
              return asText({ ok: true, message_id: p.message_id, chat_id: target, note: 'already delivered — duplicate suppressed, nothing re-sent' })
            }
          } catch {
            /* skip */
          }
        }
      }
    } catch (e) {
      console.error('[telegram-send] dup check failed (sending anyway):', e)
    }
    const check = isKnownGuestChat(target)
    if (!check.known) {
      audit({ tool: 'send_to_guest', chat_id: target, ok: false, refused: 'unknown-chat', chars: text.length })
      console.error(`[telegram-send] REFUSED unknown chat ${target}`)
      return asText({
        ok: false,
        error: `chat ${target} is not a known guest chat — only bound guests, reservation chats, or escalation sources are reachable. Re-check the escalation/guest entity for the right id.`,
      })
    }
    if (!TOKEN) return asText({ ok: false, error: 'TELEGRAM_BOT_TOKEN not set' })
    try {
      const r = await fetch(`https://api.telegram.org/bot${TOKEN}/sendMessage`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ chat_id: Number(target), text }),
        signal: AbortSignal.timeout(15000),
      })
      const j: any = await r.json().catch(() => ({}))
      const ok = r.ok && j?.ok !== false
      audit({ tool: 'send_to_guest', chat_id: target, ok, via: check.via, message_id: j?.result?.message_id ?? null, chars: text.length, error: ok ? null : JSON.stringify(j).slice(0, 300) })
      if (!ok) {
        console.error(`[telegram-send] send FAILED chat=${target}: ${r.status}`)
        return asText({ ok: false, status: r.status, error: JSON.stringify(j).slice(0, 300) })
      }
      console.error(`[telegram-send] delivered message_id=${j?.result?.message_id} to ${target} (via ${check.via})`)
      return asText({ ok: true, message_id: j?.result?.message_id, chat_id: target })
    } catch (e) {
      audit({ tool: 'send_to_guest', chat_id: target, ok: false, chars: text.length, error: String(e).slice(0, 300) })
      return asText({ ok: false, error: String(e).slice(0, 300) })
    }
  },
)

await server.connect(new StdioServerTransport())
console.error(`[telegram-send] ready; sibyl=${SIBYL_DB} escalations=${ESCALATION_LOG}`)
