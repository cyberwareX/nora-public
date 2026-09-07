/**
 * Twitter capability — a standalone **stdio MCP server** the bridge spawns and the OpenClaude
 * SDK connects to (the in-process `sdk` server type fails to instantiate in this SDK build, so
 * we use the well-supported stdio transport). Exposes `mcp__twitter__post` / `mcp__twitter__reply`
 * to Express; the Rust wall still gates every call via the bridge's `canUseTool`.
 *
 * The bearer is `X_BEARER_TOKEN` (injected by the harness ONLY for routes whose `secrets: [x]`
 * grant it). Dry-run is enforced at the **Rust wall** (`config.dry_run`), which denies the post call
 * before it reaches here — so this server always posts for real when actually invoked. stdout is the
 * MCP protocol channel: NEVER log to it — use stderr.
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { z } from 'zod'

const API = 'https://api.twitter.com/2'
const bearer = () => process.env.X_BEARER_TOKEN
const authHeaders = (json = false) => ({
  Authorization: `Bearer ${bearer()}`,
  ...(json ? { 'Content-Type': 'application/json' } : {}),
})

async function xPostTweet(body: Record<string, unknown>): Promise<unknown> {
  if (!bearer()) return { ok: false, error: 'X_BEARER_TOKEN not set — no act-secret was injected for this route' }
  const r = await fetch(`${API}/tweets`, { method: 'POST', headers: authHeaders(true), body: JSON.stringify(body) })
  const j: any = await r.json().catch(() => ({}))
  if (!r.ok) return { ok: false, status: r.status, error: JSON.stringify(j).slice(0, 400) }
  console.error('[twitter-mcp] posted id=', j?.data?.id)
  return { ok: true, id: j?.data?.id, text: j?.data?.text }
}

// The authenticated user's id — needed for the /users/:id/retweets endpoint. Resolved once via /me.
let meId: string | null = null
async function myId(): Promise<{ id?: string; error?: unknown }> {
  if (meId) return { id: meId }
  if (!bearer()) return { error: { ok: false, error: 'X_BEARER_TOKEN not set' } }
  const r = await fetch(`${API}/users/me`, { headers: authHeaders() })
  const j: any = await r.json().catch(() => ({}))
  if (!r.ok || !j?.data?.id) return { error: { ok: false, status: r.status, error: JSON.stringify(j).slice(0, 300) } }
  meId = j.data.id as string
  return { id: meId }
}

const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(v) }] })

const server = new McpServer({ name: 'twitter', version: '0.1.0' })

server.registerTool(
  'post',
  {
    description: 'Post a NEW standalone tweet as @agentdack (≤280 chars). Use for your own posts, not replies.',
    inputSchema: { text: z.string().min(1).max(280) },
  },
  async ({ text }: { text: string }) => asText(await xPostTweet({ text })),
)

server.registerTool(
  'reply',
  {
    description:
      'Reply to a tweet (≤280 chars). `in_reply_to_tweet_id` is the source_tweet_id from your baton context.',
    inputSchema: { text: z.string().min(1).max(280), in_reply_to_tweet_id: z.string().min(1) },
  },
  async ({ text, in_reply_to_tweet_id }: { text: string; in_reply_to_tweet_id: string }) =>
    asText(await xPostTweet({ text, reply: { in_reply_to_tweet_id } })),
)

server.registerTool(
  'quote_tweet',
  {
    description:
      'Quote-tweet: post your own take (≤280) WITH a tweet embedded below it. Use RARELY — only to amplify ' +
      'something genuinely signal-worth, with your own value on top. `quote_tweet_id` is the tweet to quote.',
    inputSchema: { text: z.string().min(1).max(280), quote_tweet_id: z.string().min(1) },
  },
  async ({ text, quote_tweet_id }: { text: string; quote_tweet_id: string }) =>
    asText(await xPostTweet({ text, quote_tweet_id })),
)

server.registerTool(
  'retweet',
  {
    description:
      'Retweet a tweet (boost it to your timeline, no comment). RARE — your retweets are an endorsement; ' +
      'amplify only the genuinely good. `tweet_id` is the tweet to boost. (To add your own words, quote instead.)',
    inputSchema: { tweet_id: z.string().min(1) },
  },
  async ({ tweet_id }: { tweet_id: string }) => {
    const { id, error } = await myId()
    if (!id) return asText(error)
    const r = await fetch(`${API}/users/${id}/retweets`, {
      method: 'POST',
      headers: authHeaders(true),
      body: JSON.stringify({ tweet_id }),
    })
    const j: any = await r.json().catch(() => ({}))
    if (!r.ok) return asText({ ok: false, status: r.status, error: JSON.stringify(j).slice(0, 400) })
    console.error('[twitter-mcp] retweeted', tweet_id)
    return asText({ ok: true, retweeted: j?.data?.retweeted ?? true, tweet_id })
  },
)

server.registerTool(
  'unretweet',
  {
    description: 'Undo a retweet of `tweet_id` (e.g. you boosted something that aged badly).',
    inputSchema: { tweet_id: z.string().min(1) },
  },
  async ({ tweet_id }: { tweet_id: string }) => {
    const { id, error } = await myId()
    if (!id) return asText(error)
    const r = await fetch(`${API}/users/${id}/retweets/${tweet_id}`, { method: 'DELETE', headers: authHeaders() })
    const j: any = await r.json().catch(() => ({}))
    if (!r.ok) return asText({ ok: false, status: r.status, error: JSON.stringify(j).slice(0, 400) })
    return asText({ ok: true, retweeted: j?.data?.retweeted ?? false, tweet_id })
  },
)

await server.connect(new StdioServerTransport())
console.error('[twitter-mcp] ready')
