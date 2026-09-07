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
import { Database } from 'bun:sqlite'

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
  const raw = readFirst([name, `config/${name}`, `../${name}`])
  return raw ? { ...def, ...JSON.parse(raw) } : def
}

/** Resolve a message's webhook path by sender precedence: operator → trusted group → public. */
function routeFor(cfg: Cfg, fromId: number | null, chatId: number): { path: string; why: string } {
  // operators/delegators: an `operator_ids` list (preferred) OR the single `operator_user_id` (legacy).
  const ops: number[] = (Array.isArray((cfg as any).operator_ids) && (cfg as any).operator_ids.length)
    ? (cfg as any).operator_ids
    : (cfg.operator_user_id != null ? [cfg.operator_user_id] : [])
  if (fromId != null && ops.includes(fromId)) return { path: cfg.op_path, why: 'OP→org' }
  const group = cfg.groups[String(chatId)]
  if (group) return { path: group, why: `group→${group}` }
  return { path: cfg.pub_path, why: 'pub' }
}

const cfg = loadConfig()
const TOKEN = loadToken()
const bot = new Bot(TOKEN)

// --- Voice-note transcription (opt-in per ingress config; 2026-08-15) ---
// Config: "voice_stt": {"key_file": "/path/to/xiaomi_api_key", "model": "mimo-v2.5-asr",
//         "api_url": "https://api.xiaomimimo.com/v1", "max_seconds": 120}
// Without the block, voice messages forward as a labeled placeholder (daemon still wakes and can
// ask the guest to type). With it, the OGG is fetched from Telegram and transcribed via Xiaomi
// MiMo ASR (OpenAI-style input_audio on /chat/completions), so the daemon sees the words.
async function transcribeVoice(fileId: string, durationSec: number): Promise<string | null> {
  const vs = (cfg as any).voice_stt
  if (!vs?.key_file) return null
  if (durationSec > (vs.max_seconds ?? 120)) {
    console.error(`[tg-ingress] voice ${durationSec}s exceeds max_seconds — skipping STT`)
    return null
  }
  try {
    const key = readFileSync(vs.key_file, 'utf8').trim().split('\n')[0].trim()
    const f = await bot.api.getFile(fileId)
    const audioRes = await fetch(`https://api.telegram.org/file/bot${TOKEN}/${f.file_path}`)
    const audio = Buffer.from(await audioRes.arrayBuffer())
    const ext = (f.file_path?.split('.').pop() ?? 'ogg').toLowerCase()
    const format = ext === 'oga' ? 'ogg' : ext

    let r: Response, text: string | undefined, j: any
    if (vs.provider === 'groq') {
      // Groq-hosted Whisper (free tier; key shared with the proxy's LLM fallback lane)
      const fd = new FormData()
      fd.append('model', vs.model ?? 'whisper-large-v3-turbo')
      fd.append('file', new Blob([audio]), `voice.${format}`)
      r = await fetch(vs.api_url ?? 'https://api.groq.com/openai/v1/audio/transcriptions', {
        method: 'POST',
        headers: { Authorization: `Bearer ${key}` },
        body: fd,
      })
      j = await r.json()
      text = j?.text
    } else {
      // Xiaomi MiMo ASR (OpenAI-style input_audio on /chat/completions)
      r = await fetch((vs.api_url ?? 'https://api.xiaomimimo.com/v1') + '/chat/completions', {
        method: 'POST',
        headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          model: vs.model ?? 'mimo-v2.5-asr',
          messages: [{ role: 'user', content: [{ type: 'input_audio', input_audio: { data: audio.toString('base64'), format } }] }],
        }),
      })
      j = await r.json()
      text = j?.choices?.[0]?.message?.content
    }
    if (!r.ok || typeof text !== 'string' || !text.trim()) {
      console.error(`[tg-ingress] voice STT failed (${r.status}): ${JSON.stringify(j).slice(0, 200)}`)
      return null
    }
    return text.trim()
  } catch (e) {
    console.error('[tg-ingress] voice STT error:', e)
    return null
  }
}

// --- Batch Group Message Storage ---
// Group/supergroup messages are saved to a separate SQLite DB and do NOT wake the daemon.
// DM (private) messages still POST to the webhook for real-time processing.
// Daily group-digest stimulus (cron 9 AM) reads unprocessed rows and batches them.
const excludedGroups: number[] = (cfg as any).excluded_groups ?? []
const groupDb = new Database('group_messages.sqlite')
groupDb.run(`CREATE TABLE IF NOT EXISTS group_messages (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  chat_id         INTEGER NOT NULL,
  chat_title      TEXT,
  chat_type       TEXT NOT NULL,
  message_id      INTEGER NOT NULL,
  from_user_id    INTEGER,
  from_username   TEXT,
  from_first_name TEXT,
  text            TEXT NOT NULL,
  sent_at         TEXT NOT NULL,
  received_at     TEXT NOT NULL,
  processed       INTEGER DEFAULT 0,
  daemon          TEXT DEFAULT 'nora'
)`)
groupDb.run('CREATE INDEX IF NOT EXISTS idx_gm_processed ON group_messages(processed)')
groupDb.run('CREATE INDEX IF NOT EXISTS idx_gm_chat ON group_messages(chat_id)')
groupDb.run('CREATE INDEX IF NOT EXISTS idx_gm_user ON group_messages(from_user_id)')
groupDb.run('CREATE INDEX IF NOT EXISTS idx_gm_received ON group_messages(received_at)')
const insertGroupMsg = groupDb.prepare(
  `INSERT INTO group_messages (chat_id, chat_title, chat_type, message_id, from_user_id, from_username, from_first_name, text, sent_at, received_at, processed, daemon)
   VALUES ($chat_id, $chat_title, $chat_type, $message_id, $from_user_id, $from_username, $from_first_name, $text, $sent_at, $received_at, 0, 'nora')`
)
console.error('[tg-ingress] group_messages.sqlite initialized — group messages will be batched, DMs wake in real-time')

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

  // Voice notes: transcribe (when configured) so the daemon sees words, not an empty message.
  let voiceText: string | null = null
  if ((m as any).voice) {
    const v = (m as any).voice
    const transcript = await transcribeVoice(v.file_id, v.duration ?? 0)
    voiceText = transcript
      ? `[voice message, ${v.duration ?? '?'}s — transcribed] ${transcript}`
      : '[voice message received — transcription unavailable; ask the sender to type it out]'
    console.error(`[tg-ingress] voice msg ${m.message_id} (${v.duration}s) → ${transcript ? 'transcribed ' + transcript.length + ' chars' : 'NO transcript'}`)
  }

  // --- Batch: group/supergroup messages → save to SQLite, skip webhook POST ---
  const chatType = m.chat.type
  const isGroup = chatType === 'group' || chatType === 'supergroup'
  if (isGroup && !excludedGroups.includes(m.chat.id)) {
    try {
      insertGroupMsg.run({
        $chat_id: m.chat.id,
        $chat_title: (m as any).chat.title ?? null,
        $chat_type: chatType,
        $message_id: m.message_id,
        $from_user_id: fromId,
        $from_username: m.from?.username ?? null,
        $from_first_name: m.from?.first_name ?? null,
        $text: voiceText ?? (m as any).text ?? (m as any).caption ?? '',
        $sent_at: sent_at ?? new Date().toISOString(),
        $received_at: new Date().toISOString(),
      })
      console.error(`[tg-ingress] BATCHED group msg ${m.message_id} from @${m.from?.username} chat ${m.chat.id} (${chatType}) — saved, no wake`)
    } catch (e) {
      console.error('[tg-ingress] group save failed:', e)
    }
    return // Skip webhook POST — daemon stays asleep for group messages
  }

  // Deep-link capture: a guest opening t.me/<bot>?start=<booking-code> sends "/start <code>".
  // The validated code rides the payload as `start_param` — the agent binds THIS chat to that
  // reservation in memory (Sibyl `guest` entity + active-stay state). Charset-limited to what a
  // real Telegram deep link can produce; anything else is untrusted text and stays out.
  const rawText: string = (m as any).text ?? ''
  const startMatch = /^\/start(?:@\w+)?\s+(\S+)/.exec(rawText.trim())
  const start_param =
    startMatch && /^[A-Za-z0-9_-]{1,64}$/.test(startMatch[1]) ? startMatch[1] : null

  const body = {
    chat_id: m.chat.id,
    message_id: m.message_id,
    text: voiceText ?? (m as any).text ?? (m as any).caption ?? '',
    from_username: m.from?.username ?? null,
    from_user_id: fromId,
    chat_type: m.chat.type,
    sent_at,
    start_param,
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
