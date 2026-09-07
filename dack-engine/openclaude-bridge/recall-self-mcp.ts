/**
 * Runlog SELF-RECALL capability — a `trust: self` sibling of `runlog-recall-mcp.ts`. It returns ONLY the
 * duck's OWN post-firebreak output: its `thought`, what it `sent`, the conversation tags, and each entry's
 * ORIGIN TRUST — and it NEVER renders the raw untrusted `incoming`. Because the firebreak doctrine treats
 * the duck's digested thought (not the raw payload) as the trust boundary, reading this server does NOT
 * lower the cycle's trust — so a `self`-trust cron (the activity-digest) can mine recent chat activity and
 * still write `memory/` at self-trust.
 *
 * Contrast with the raw `recall` server (`trust: public`): that one renders verbatim incoming text, so
 * reading it taints the cycle to public. Both are granted at the perceive tier — the model CHOOSES the
 * verbatim transcript (and accepts the public taint) vs this clean thoughts-only view.
 *
 * stdout is the MCP protocol channel — NEVER log to it; use stderr. Pure logic lives in `runlog-recall.ts`.
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { z } from 'zod'
import {
  atLeastTrust,
  clamp,
  cursorPage,
  findEntry,
  fmtTime,
  listRecentTags,
  loadEntries,
  readTagNotes,
  recentActivity,
  recentDates,
  renderEntryFull,
  renderThoughtLog,
} from './runlog-recall'

const RUNLOG_DIR =
  process.env.RECALL_DIR ??
  (process.env.DACK_BRIDGE_CWD ? `${process.env.DACK_BRIDGE_CWD}/runlogs` : 'runlogs')
// How far back `read_entry` looks up a run_id (covers the widest recall window).
const READ_DAYS = 30
const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(v) }] })

const server = new McpServer({ name: 'recall-self', version: '0.1.0' })

server.registerTool(
  'recent_activity',
  {
    description:
      "Your OWN recent reasoning + replies across ALL your chats (default last 2 days), newest last — your " +
      "thoughts and what you said, each tagged with its run_id + chat + origin trust. NEVER the raw incoming " +
      "text, so reading it keeps you clean (self-trust). Use it to take stock of what's been happening. " +
      "Long thoughts are TRUNCATED with a `[+N chars — read_entry …]` marker — pass that run_id to " +
      "`read_entry` for the full one. Pages NEWEST first: omit `cursor` for the latest `limit`, then pass " +
      "back the reply's `page.cursor` for the next (older) page (null = no more). `min_trust` " +
      '(self|org|public) floors out lower-trust chats (e.g. set `org` to ignore strangers).',
    inputSchema: {
      days: z.number().int().optional(),
      min_trust: z.string().optional(),
      limit: z.number().int().optional(),
      cursor: z.string().optional(),
    },
  },
  async ({ days, min_trust, limit, cursor }: { days?: number; min_trust?: string; limit?: number; cursor?: string }) => {
    const d = clamp(days, 2, 30)
    const page = cursorPage(recentActivity(RUNLOG_DIR, d, min_trust), clamp(limit, 25, 50), cursor)
    return asText({
      ok: true,
      days: d,
      min_trust: min_trust ?? null,
      page: { total: page.total, returned: page.returned, has_more: page.has_more, cursor: page.cursor },
      log: renderThoughtLog(page.items),
    })
  },
)

server.registerTool(
  'recall_self_by_tag',
  {
    description:
      'Your OWN thoughts + replies in ONE chat/topic by its tag (find tags with `list_recent_tags`), over ' +
      'the recent window. Thoughts-only (no raw incoming) — stays self-trust. Long thoughts are truncated ' +
      "with a `read_entry` marker; pages newest first (omit `cursor`, then pass back `page.cursor`). " +
      '`min_trust` optional floor.',
    inputSchema: {
      tag: z.string().min(1),
      days: z.number().int().optional(),
      min_trust: z.string().optional(),
      limit: z.number().int().optional(),
      cursor: z.string().optional(),
    },
  },
  async ({ tag, days, min_trust, limit, cursor }: { tag: string; days?: number; min_trust?: string; limit?: number; cursor?: string }) => {
    const all = loadEntries(RUNLOG_DIR, recentDates(RUNLOG_DIR, clamp(days, 7, 30)))
    const matched = atLeastTrust(all.filter((e) => e.tags.includes(tag) && (e.thought || e.sent.length)), min_trust)
    const page = cursorPage(matched, clamp(limit, 25, 50), cursor)
    return asText({
      ok: true,
      tag,
      page: { total: page.total, returned: page.returned, has_more: page.has_more, cursor: page.cursor },
      log: renderThoughtLog(page.items),
    })
  },
)

server.registerTool(
  'read_entry',
  {
    description:
      'Read ONE of your runlog entries IN FULL by its run_id — the bracketed id shown in ' +
      '`recent_activity` / `recall_self_by_tag` (use it when a thought was truncated with a `[+N chars]` ' +
      'marker and you want the whole thing). Returns your full thought + what you sent for that cycle, ' +
      'NEVER the raw incoming text, so it stays self-trust.',
    inputSchema: { run_id: z.string().min(1) },
  },
  async ({ run_id }: { run_id: string }) => {
    const e = findEntry(RUNLOG_DIR, run_id, READ_DAYS)
    if (!e) return asText({ ok: false, error: `entry "${run_id}" not found in the last ${READ_DAYS} days` })
    return asText({ ok: true, run_id, tag: e.tags[0] ?? null, trust: e.trust || null, at: fmtTime(e.timestamp), entry: renderEntryFull(e) })
  },
)

server.registerTool(
  'list_recent_tags',
  {
    description:
      'List your recent conversation tags (default last 7 days), newest first, with a who-was-here hint — ' +
      'metadata only (no message content), to find a tag for `recall_self_by_tag`.',
    inputSchema: { days: z.number().int().optional(), limit: z.number().int().optional() },
  },
  async ({ days, limit }: { days?: number; limit?: number }) => {
    const entries = loadEntries(RUNLOG_DIR, recentDates(RUNLOG_DIR, clamp(days, 7, 60)))
    return asText({ ok: true, tags: listRecentTags(entries, clamp(limit, 20, 100)) })
  },
)

server.registerTool(
  'read_tag_notes',
  {
    description:
      'Read your tag-notes catalogue — the latest "sticky note" per conversation (who they are, open ' +
      'threads, what to remember), newest first, each with its subject trust + when. This is your quick ' +
      "orientation on a chat before you reply, and the digest's working set. `tag` reads one chat; " +
      '`min_trust` (self|org|public) floors out lower-trust subjects.',
    inputSchema: { tag: z.string().optional(), min_trust: z.string().optional() },
  },
  async ({ tag, min_trust }: { tag?: string; min_trust?: string }) => {
    const notes = readTagNotes(RUNLOG_DIR, { tag, minTrust: min_trust }).map((n) => ({
      tag: n.tag,
      note: n.note,
      trust: n.trust,
      at: fmtTime(n.timestamp),
    }))
    return asText({ ok: true, notes })
  },
)

await server.connect(new StdioServerTransport())
const ready_n = (() => { try { return recentActivity(RUNLOG_DIR, 2).length } catch { return -1 } })()
console.error(`[recall-self-mcp] ready (dir=${RUNLOG_DIR}, ${ready_n} recent entries)`)
