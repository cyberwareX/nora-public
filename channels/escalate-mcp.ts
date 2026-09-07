/**
 * Escalate — the agent's ONE path to humans who are not the current guest: the owner (default)
 * and the cleaning crew (`audience: cleaners`). A **notify-lane producer**, not a messenger: it
 * POSTs to the local notify router (connector-agnostic — telegram/email/whatever the operator
 * configured) and appends to a local JSONL so nothing is lost even if the router is down.
 *
 * Deliberately callable from PUBLIC-taint guest cycles: the model chooses only severity + text —
 * the DESTINATIONS are operator config, so there is nothing to hijack. (Design lesson from a
 * sibling deployment whose escalation path was a memory-file write the trust wall refused.)
 *
 * stdout is the MCP protocol channel — NEVER log to it; use stderr.
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { appendFileSync } from 'node:fs'
import { z } from 'zod'

const ROUTER = process.env.NOTIFY_URL ?? 'http://127.0.0.1:8794/notify'
const LOG = process.env.ESCALATION_LOG ?? 'escalations.jsonl'

const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(v) }] })

const server = new McpServer({ name: 'escalate', version: '0.1.0' })

server.registerTool(
  'escalate',
  {
    description:
      'Notify a human. `audience` "op" (default) reaches the owner; "cleaners" reaches the cleaning ' +
      'crew. `severity`: "escalation" (a guest needs a human — default), "alert" (something is wrong), ' +
      '"info" (a briefing, e.g. new booking or daily notice). One clear line in `message`: unit, who, ' +
      'what, by when. Destinations are fixed by the operator — you choose only the text. Escalating ' +
      'IS handling it: after this returns ok, the human side is covered.',
    inputSchema: {
      message: z.string().min(1).max(2000),
      severity: z.enum(['escalation', 'alert', 'info']).optional(),
      audience: z.enum(['op', 'cleaners']).optional(),
    },
  },
  async ({ message, severity, audience }: { message: string; severity?: string; audience?: string }) => {
    const n = {
      severity: severity ?? 'escalation',
      audience: audience ?? 'op',
      source: 'agent',
      title: `Nora ${severity ?? 'escalation'}`,
      body: message,
      source_chat: process.env.ESCALATE_SOURCE_CHAT ?? null,
      at: Math.floor(Date.now() / 1000),
    }
    // Durable first — the JSONL survives a down router (and IS the demo's audit exhibit).
    try {
      appendFileSync(LOG, JSON.stringify(n) + '\n')
    } catch (e) {
      console.error('[escalate] log append failed:', e)
    }
    try {
      const r = await fetch(ROUTER, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(n),
        signal: AbortSignal.timeout(5000),
      })
      console.error(`[escalate] ${n.severity}/${n.audience} → router (${r.status})`)
      return asText({ ok: true, delivered: r.ok, logged: true })
    } catch (e) {
      // Logged locally; the router (or its operator) reconciles from the JSONL.
      console.error('[escalate] router unreachable (logged locally):', e)
      return asText({ ok: true, delivered: false, logged: true, note: 'queued locally; router down' })
    }
  },
)

await server.connect(new StdioServerTransport())
console.error('[escalate] ready →', ROUTER)
