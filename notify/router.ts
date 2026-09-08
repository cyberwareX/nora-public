/**
 * Op-notify router — ONE lane for everything a human must hear about: agent escalations
 * (escalate MCP), ingest events, and harness failures (engine `notify_url` hook). Producers POST
 * here; CONNECTORS deliver. Telegram + jsonl today; email/buzz are just more connectors — the
 * `{severities, deliver}` shape is the plugin seam this design feeds back into dack v2.
 *
 * POST /notify {severity: escalation|alert|error|info, audience?: op|cleaners, source, title, body}
 *
 * Hygiene (lessons from live incidents upstream): dedup window so one brownout isn't 50 identical
 * pings; an hourly delivery cap with an explicit "storm suppressed" notice (silent suppression is
 * how operators stop trusting a lane); jsonl audit of EVERY notification, delivered or not.
 */
import { appendFileSync, readFileSync } from 'node:fs'

type Notification = {
  severity: 'escalation' | 'alert' | 'error' | 'info'
  audience?: string
  source?: string
  title?: string
  body?: string
  /** The guest chat behind an escalation — surfaced in the delivered text so the operator's
   *  answer can name it and the relay tool can verify it. */
  source_chat?: string | null
}

type Cfg = {
  listen: string
  op_chat_ids: number[] // legacy alias for audiences.owner
  cleaners_chat_ids: number[] // legacy alias for audiences.cleaners
  /** audience → telegram chat ids. Unknown/absent audience falls back to owner. */
  audiences: Record<string, number[]>
  // severity → connector list. "telegram" delivers; "log" is jsonl-only (always logged anyway).
  severity_routes: Record<string, string[]>
  dedup_window_sec: number
  hourly_cap: number
}

const DEF: Cfg = {
  listen: '127.0.0.1:8794',
  op_chat_ids: [],
  cleaners_chat_ids: [],
  audiences: {},
  severity_routes: { escalation: ['telegram'], alert: ['telegram'], error: ['telegram'], info: ['log'] },
  dedup_window_sec: 600,
  hourly_cap: 20,
}

function loadCfg(): Cfg {
  // Root-first like the ingress: a gitignored ./notify.config.json (real chat ids) wins over the
  // committed config/notify.config.json (placeholders), so operators never commit their ids.
  const names = process.env.NOTIFY_CONFIG
    ? [process.env.NOTIFY_CONFIG]
    : ['notify.config.json', 'config/notify.config.json']
  for (const name of names) {
    try {
      return { ...DEF, ...JSON.parse(readFileSync(name, 'utf8')) }
    } catch {
      /* next */
    }
  }
  console.error(`[notify] no config found (${names.join(', ')}) — jsonl-only mode`)
  return DEF
}

const cfg = loadCfg()
const TOKEN = (process.env.TELEGRAM_BOT_TOKEN ?? '').trim()
const LOG = process.env.NOTIFY_LOG ?? 'notify/notify-log.jsonl'

const ICON: Record<string, string> = { escalation: '🔔', alert: '⚠️', error: '❌', info: 'ℹ️' }
const recent = new Map<string, number>() // dedup: key → last-delivered epoch
let hourWindow: number[] = [] // delivery timestamps for the rolling cap
let stormNoticed = false

async function tgSend(chatId: number, text: string): Promise<boolean> {
  if (!TOKEN) return false
  try {
    const r = await fetch(`https://api.telegram.org/bot${TOKEN}/sendMessage`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ chat_id: chatId, text }),
      signal: AbortSignal.timeout(10000),
    })
    if (!r.ok) console.error(`[notify] telegram ${chatId} → ${r.status}`)
    return r.ok
  } catch (e) {
    console.error('[notify] telegram send failed:', e)
    return false
  }
}

async function deliver(n: Notification): Promise<{ delivered: boolean; suppressed?: string }> {
  const now = Date.now() / 1000
  const routes = cfg.severity_routes[n.severity] ?? ['log']
  if (!routes.includes('telegram')) return { delivered: false }

  const key = `${n.severity}|${n.audience}|${n.title}|${n.body}`
  const last = recent.get(key)
  if (last && now - last < cfg.dedup_window_sec) return { delivered: false, suppressed: 'duplicate' }

  hourWindow = hourWindow.filter((t) => now - t < 3600)
  if (hourWindow.length >= cfg.hourly_cap) {
    if (!stormNoticed) {
      stormNoticed = true
      for (const id of cfg.op_chat_ids)
        await tgSend(id, `⚠️ notify storm: over ${cfg.hourly_cap}/h — further notifications go to the log only`)
    }
    return { delivered: false, suppressed: 'storm-cap' }
  }
  stormNoticed = false

  const aud = { owner: cfg.op_chat_ids, cleaners: cfg.cleaners_chat_ids, ...cfg.audiences }
  const chats = (n.audience && aud[n.audience]?.length ? aud[n.audience] : aud['owner']) ?? []
  const text = `${ICON[n.severity] ?? ''} ${n.title ?? n.severity}\n${n.body ?? ''}${n.source_chat ? `\nguest chat: ${n.source_chat}` : ''}${n.source ? `\n— ${n.source}` : ''}`
  let ok = false
  for (const id of chats) ok = (await tgSend(id, text)) || ok
  if (ok) {
    recent.set(key, now)
    hourWindow.push(now)
  }
  return { delivered: ok }
}

Bun.serve({
  hostname: cfg.listen.split(':')[0],
  port: Number(cfg.listen.split(':')[1] ?? 8794),
  async fetch(req: Request) {
    const url = new URL(req.url)
    if (req.method === 'POST' && url.pathname === '/notify') {
      let n: Notification
      try {
        n = (await req.json()) as Notification
      } catch {
        return new Response('bad json', { status: 400 })
      }
      if (!['escalation', 'alert', 'error', 'info'].includes(n.severity)) {
        return new Response('bad severity', { status: 400 })
      }
      const result = await deliver(n)
      try {
        appendFileSync(LOG, JSON.stringify({ at: Math.floor(Date.now() / 1000), ...n, ...result }) + '\n')
      } catch (e) {
        console.error('[notify] log append failed:', e)
      }
      console.error(`[notify] ${n.severity}/${n.audience ?? 'op'} "${n.title}" → ${JSON.stringify(result)}`)
      return Response.json({ ok: true, ...result })
    }
    if (req.method === 'GET' && url.pathname === '/health') return Response.json({ ok: true })
    return new Response('not found', { status: 404 })
  },
})
console.error(`[notify] listening on ${cfg.listen}; audiences=${JSON.stringify(Object.fromEntries(Object.entries({ owner: cfg.op_chat_ids, cleaners: cfg.cleaners_chat_ids, ...cfg.audiences }).map(([k,v])=>[k,(v as number[]).length])))} telegram=${TOKEN ? 'on' : 'OFF'}`)
