/**
 * Telegram INGRESS adapter — the operator-owned, grammY-driven inbound side.
 *
 * It is NOT part of the Rust harness and the harness never parses Telegram. It long-polls the bot,
 * decides each message's TRUST by *who sent it* (its own config — chat/user → a harness webhook PATH),
 * and POSTs a normalized message to the harness's localhost webhook. The path's tier (the operator's
 * generic `config.webhooks:` map in dack.config.yaml) is what the duck's cycle gets seeded at — the
 * Rust core just sees "a localhost webhook fired at tier X." All Telegram specifics live here.
 *
 * Routing (who → which webhook PATH, by precedence):
 *   1. the operator's user_id (anywhere, even inside a public group) → `op_path` (org) — provenance
 *      follows the person, not the room.
 *   2. a known/trusted GROUP by chat_id (the `groups` map) → that group's configured path (e.g. a
 *      private team/investor group → an org path). All members of a trusted group inherit its tier.
 *   3. everyone else (strangers, public/trencher groups) → `pub_path` (public).
 * The PATH's tier lives in the harness `config.webhooks:` map — this adapter only assigns the path.
 * Run by the harness `modules:` supervisor (no manual start).
 */
import { Bot } from 'grammy'
import { readFileSync } from 'node:fs'

function readFirst(paths: string[]): string | null {
  for (const p of paths) {
    try {
      return readFileSync(p, 'utf8')
    } catch {
      /* next */
    }
  }
  return null
}

function loadToken(): string {
  const t = process.env.TELEGRAM_BOT_TOKEN ?? readFirst(['secrets/telegram.token', '../secrets/telegram.token'])
  if (!t) throw new Error('no TELEGRAM_BOT_TOKEN env and no secrets/telegram.token')
  return t.trim()
}

type Cfg = {
  harness_webhook: string
  operator_user_id: number | null
  op_path: string
  pub_path: string
  // Trusted/known groups: chat_id (as a string key) → the webhook path its members route to.
  groups: Record<string, string>
}
function loadConfig(): Cfg {
  const def: Cfg = {
    harness_webhook: 'http://127.0.0.1:8787',
    operator_user_id: null,
    op_path: '/telegram/op',
    pub_path: '/telegram/pub',
    groups: {},
  }
  const name = process.env.TELEGRAM_INGRESS_CONFIG ?? 'telegram-ingress.config.json'
  const raw = readFirst([name, `openclaude-bridge/${name}`, `../${name}`])
  return raw ? { ...def, ...JSON.parse(raw) } : def
}

/** Resolve a message's webhook path by sender precedence: operator → trusted group → public. */
function routeFor(cfg: Cfg, fromId: number | null, chatId: number): { path: string; why: string } {
  if (cfg.operator_user_id != null && fromId === cfg.operator_user_id) return { path: cfg.op_path, why: 'OP→org' }
  const group = cfg.groups[String(chatId)]
  if (group) return { path: group, why: `group→${group}` }
  return { path: cfg.pub_path, why: 'pub' }
}

const cfg = loadConfig()
const bot = new Bot(loadToken())

bot.on('message', async (ctx) => {
  const m = ctx.message
  const fromId = m.from?.id ?? null
  const { path, why } = routeFor(cfg, fromId, m.chat.id)
  // When the user ACTUALLY sent this message (Telegram `m.date`, unix seconds) → a readable UTC
  // string the model can compare against its `now` block. A coalesced wake keeps each message's
  // `sent_at` in `items[]`, so the model can tell messages that arrived WHILE it was thinking/replying
  // from genuine impatient repeats — and see the real gaps between them.
  const sent_at = m.date
    ? new Date(m.date * 1000).toISOString().replace('T', ' ').replace(/\..+Z$/, ' UTC')
    : null
  const body = {
    chat_id: m.chat.id,
    message_id: m.message_id,
    text: (m as any).text ?? (m as any).caption ?? '',
    from_username: m.from?.username ?? null,
    from_user_id: fromId,
    chat_type: m.chat.type,
    sent_at,
    // The THREAD key = the chat id (NOT per-message). The harness coalesces by this (a chat's
    // messages fold into one debounced wake) and keys the chat's sticky session on it. The
    // per-message id stays in `message_id` above (for reply-targeting + context).
    dedup_key: String(m.chat.id),
  }
  try {
    const r = await fetch(cfg.harness_webhook + path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    console.error(`[tg-ingress] ${why} msg ${m.message_id} from @${m.from?.username} chat ${m.chat.id} (${m.chat.type}) → ${path} (${r.status})`)
  } catch (e) {
    console.error('[tg-ingress] forward failed:', e)
  }
})

bot.catch((err) => console.error('[tg-ingress] bot error:', err?.error ?? err))
console.error(`[tg-ingress] long-polling; operator_user_id=${cfg.operator_user_id} → ${cfg.op_path}; trusted groups=${JSON.stringify(cfg.groups)}; else → ${cfg.pub_path}; harness=${cfg.harness_webhook}`)
bot.start()
