/**
 * Twitter READ capability — a standalone **stdio MCP server** the bridge spawns, sibling of
 * `twitter-mcp.ts` (the write side). This lets the duck PULL context on demand in Perceive
 * (look up a user, fetch a thread, search) instead of only reacting to what the sensors push.
 *
 * Registered `tier: read`, `trust: public` (reading the public timeline contaminates the cycle to
 * public → the wall floors it at Express — reading can never enable a trade). The Rust wall still
 * gates every call via the bridge's `canUseTool`; the per-server `tools` allowlist holds this server
 * to its read surface fail-closed. Same `X_BEARER_TOKEN` as the sensors + the write server.
 *
 * stdout is the MCP protocol channel — NEVER log to it; use stderr. Mirrors the X-API-v2 GET
 * patterns in `soul-template/skills/twitter/scripts/x_api.py` (the harness-side sensor client).
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { z } from 'zod'

const API = 'https://api.twitter.com/2'

async function xGet(path: string, params: Record<string, string | number>): Promise<any> {
  const token = process.env.X_BEARER_TOKEN
  if (!token) return { ok: false, error: 'X_BEARER_TOKEN not set — no read-secret injected for this route' }
  const qs = new URLSearchParams(Object.entries(params).map(([k, v]) => [k, String(v)])).toString()
  const r = await fetch(`${API}${path}${qs ? `?${qs}` : ''}`, {
    headers: { Authorization: `Bearer ${token}` },
  })
  const j: any = await r.json().catch(() => ({}))
  if (!r.ok) return { ok: false, status: r.status, error: JSON.stringify(j).slice(0, 400) }
  return { ok: true, ...j }
}

// X search/timeline endpoints require max_results in [5,100]; clamp so the model can't 400 us.
const clampMax = (n: number | undefined, def: number) => Math.min(100, Math.max(5, n ?? def))

const usersById = (resp: any): Record<string, any> =>
  Object.fromEntries((resp?.includes?.users ?? []).map((u: any) => [u.id, u]))

const trimTweet = (t: any, users: Record<string, any> = {}) => ({
  id: t.id,
  text: t.text,
  author: users[t.author_id]?.username,
  created_at: t.created_at,
  conversation_id: t.conversation_id,
  metrics: t.public_metrics,
})

const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(v) }] })

async function resolveUserId(username: string): Promise<{ id?: string; error?: any }> {
  const handle = username.replace(/^@/, '')
  const r = await xGet(`/users/by/username/${encodeURIComponent(handle)}`, {})
  if (!r.ok) return { error: r }
  return { id: r.data?.id }
}

const server = new McpServer({ name: 'twitter-read', version: '0.1.0' })

server.registerTool(
  'get_user',
  {
    description: "Look up an X user by @username — profile, bio, follower/tweet counts. Use to size up who you're talking to.",
    inputSchema: { username: z.string().min(1) },
  },
  async ({ username }: { username: string }) => {
    const handle = username.replace(/^@/, '')
    const r = await xGet(`/users/by/username/${encodeURIComponent(handle)}`, {
      'user.fields': 'description,public_metrics,created_at,verified',
    })
    if (!r.ok) return asText(r)
    const u = r.data ?? {}
    return asText({ ok: true, user: { id: u.id, username: u.username, name: u.name, description: u.description, metrics: u.public_metrics, verified: u.verified, created_at: u.created_at } })
  },
)

server.registerTool(
  'get_user_tweets',
  {
    description: "An @username's recent tweets (excludes their replies/retweets by default). Use to learn what someone's been saying before you engage.",
    inputSchema: { username: z.string().min(1), max_results: z.number().int().optional() },
  },
  async ({ username, max_results }: { username: string; max_results?: number }) => {
    const { id, error } = await resolveUserId(username)
    if (!id) return asText(error ?? { ok: false, error: 'user not found' })
    const r = await xGet(`/users/${id}/tweets`, {
      max_results: clampMax(max_results, 10),
      exclude: 'replies,retweets',
      'tweet.fields': 'created_at,public_metrics,conversation_id',
    })
    if (!r.ok) return asText(r)
    return asText({ ok: true, tweets: (r.data ?? []).map((t: any) => trimTweet(t)) })
  },
)

server.registerTool(
  'get_thread',
  {
    description: 'Fetch the conversation/thread for a conversation_id (e.g. the one on a mention you woke to) — the replies in context, so you reply to the room, not one line.',
    inputSchema: { conversation_id: z.string().min(1), max_results: z.number().int().optional() },
  },
  async ({ conversation_id, max_results }: { conversation_id: string; max_results?: number }) => {
    const r = await xGet('/tweets/search/recent', {
      query: `conversation_id:${conversation_id}`,
      max_results: clampMax(max_results, 20),
      'tweet.fields': 'created_at,author_id,conversation_id',
      expansions: 'author_id',
      'user.fields': 'username',
    })
    if (!r.ok) return asText(r)
    const users = usersById(r)
    return asText({ ok: true, replies: (r.data ?? []).map((t: any) => trimTweet(t, users)) })
  },
)

server.registerTool(
  'search_recent',
  {
    description: 'Search recent public tweets (last ~7 days) by an X query — e.g. a ticker, a topic, a "from:user". Read-only context-gathering; you still decide what (if anything) to say.',
    inputSchema: { query: z.string().min(1), max_results: z.number().int().optional() },
  },
  async ({ query, max_results }: { query: string; max_results?: number }) => {
    const r = await xGet('/tweets/search/recent', {
      query,
      max_results: clampMax(max_results, 10),
      'tweet.fields': 'created_at,author_id,public_metrics,conversation_id',
      expansions: 'author_id',
      'user.fields': 'username',
    })
    if (!r.ok) return asText(r)
    const users = usersById(r)
    return asText({ ok: true, query, results: (r.data ?? []).map((t: any) => trimTweet(t, users)) })
  },
)

server.registerTool(
  'get_tweet',
  {
    description: 'Fetch a single tweet by id, with its author and metrics — to read the exact thing being referenced before you respond.',
    inputSchema: { id: z.string().min(1) },
  },
  async ({ id }: { id: string }) => {
    const r = await xGet(`/tweets/${id}`, {
      'tweet.fields': 'created_at,author_id,public_metrics,conversation_id',
      expansions: 'author_id',
      'user.fields': 'username',
    })
    if (!r.ok) return asText(r)
    const users = usersById(r)
    return asText({ ok: true, tweet: trimTweet(r.data ?? {}, users) })
  },
)

const trimUser = (u: any) => ({ id: u.id, username: u.username, name: u.name })

server.registerTool(
  'get_followers',
  {
    description:
      "Who follows an @username (defaults to YOU, @agentdack). Use to notice new followers / who's in your " +
      'orbit — diff it against last time to see who newly followed. Read-only context.',
    inputSchema: { username: z.string().optional(), max_results: z.number().int().optional() },
  },
  async ({ username, max_results }: { username?: string; max_results?: number }) => {
    const { id, error } = await resolveUserId(username ?? process.env.DACK_HANDLE ?? 'agentdack')
    if (!id) return asText(error ?? { ok: false, error: 'user not found' })
    const r = await xGet(`/users/${id}/followers`, { max_results: clampMax(max_results, 50), 'user.fields': 'username,name' })
    if (!r.ok) return asText(r)
    return asText({ ok: true, followers: (r.data ?? []).map(trimUser) })
  },
)

server.registerTool(
  'get_following',
  {
    description: "Who an @username follows (defaults to YOU, @agentdack) — read-only context.",
    inputSchema: { username: z.string().optional(), max_results: z.number().int().optional() },
  },
  async ({ username, max_results }: { username?: string; max_results?: number }) => {
    const { id, error } = await resolveUserId(username ?? process.env.DACK_HANDLE ?? 'agentdack')
    if (!id) return asText(error ?? { ok: false, error: 'user not found' })
    const r = await xGet(`/users/${id}/following`, { max_results: clampMax(max_results, 50), 'user.fields': 'username,name' })
    if (!r.ok) return asText(r)
    return asText({ ok: true, following: (r.data ?? []).map(trimUser) })
  },
)

server.registerTool(
  'get_liking_users',
  {
    description:
      'Who liked a given tweet (`tweet_id`) — the nearest thing to a notifications feed (X has none). Use to ' +
      'see who engaged with one of your posts. Read-only.',
    inputSchema: { tweet_id: z.string().min(1), max_results: z.number().int().optional() },
  },
  async ({ tweet_id, max_results }: { tweet_id: string; max_results?: number }) => {
    const r = await xGet(`/tweets/${tweet_id}/liking_users`, { max_results: clampMax(max_results, 50), 'user.fields': 'username,name' })
    if (!r.ok) return asText(r)
    return asText({ ok: true, liking_users: (r.data ?? []).map(trimUser) })
  },
)

await server.connect(new StdioServerTransport())
console.error('[twitter-read-mcp] ready')
