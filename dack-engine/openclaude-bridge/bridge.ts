/**
 * DACK ↔ OpenClaude bridge — the thin TS process the Rust `OpenClaudeClient` spawns.
 * Imports the OpenClaude SDK as a normal npm dependency (`@gitlawb/openclaude/sdk`, the
 * bundled public entry), so this project is self-contained and the runtime seam is a clean
 * dependency boundary — not coupled to a vendored source tree.
 *
 * Protocol = NDJSON over stdio:
 *   Rust → bridge (stdin):
 *     {"kind":"invoke","system_prompt":..,"user_prompt":..,"disallowed_tools":[..],
 *      "allowed_tools":null,"model":..}
 *     {"kind":"decision","tool_use_id":..,"allow":bool,"message":..}   (per permission)
 *   bridge → Rust (stdout):
 *     {"kind":"permission","tool":..,"tool_use_id":..,"input":{..}}    (each canUseTool)
 *     {"kind":"result","output":{..AgentOutput..}}                     (once, at the end)
 *     {"kind":"error","message":..}
 *
 * The wall lives in Rust: every `canUseTool` event is relayed and blocks on the Rust
 * decision. Structured output = the model's FINAL message is a JSON object matching the Rust
 * `AgentOutput` (provider-agnostic; an MCP `submit` tool perturbed provider routing).
 * Run: `bun run bridge.ts`. SDK portability: swapping the import below for
 * `@anthropic-ai/claude-agent-sdk` is the corp / Claude-Code runtime path.
 */

import * as readline from 'node:readline'
import { tryParseOutput } from './parse'

// The SDK snapshots process.cwd() at MODULE-INIT and uses it for ALL tool/file path resolution
// (getCwd → pwd → STATE.cwd; options.cwd is NOT honored on the programmatic query() path). So a
// worker's relative writes (e.g. `solution.py`) would leak into the bridge's own dir instead of its
// /workspace. We chdir to the agent's workdir (handed by the harness as DACK_BRIDGE_CWD — a worker's
// workspace, or the duck's soul repo) BEFORE loading the SDK, then import it DYNAMICALLY so it
// snapshots the right cwd. The bridge is spawned fresh per invoke, so this is safe.
if (process.env.DACK_BRIDGE_CWD) {
  try {
    process.chdir(process.env.DACK_BRIDGE_CWD)
  } catch (e) {
    console.error(`bridge: chdir(${process.env.DACK_BRIDGE_CWD}) failed: ${e}`)
  }
}

// Protect the stdout protocol channel: any stray console.log (incl. the SDK's) → stderr. Set this
// BEFORE the (dynamic) SDK import so even the SDK's init-time logs are redirected off the channel.
console.log = (...a: unknown[]) => console.error('[bridge:log]', ...a)

// PINNED + PATCHED SDK: a vendored copy of @gitlawb/openclaude 0.18.0's built `sdk.mjs`, with the
// hard-coded "You are OpenClaude, a coding agent" identity prefix NEUTRALIZED (the 3 *_PREFIX consts
// emptied → filter(Boolean) drops them) so the model's system prompt is ONLY our SOUL.md. Absolute
// path via import.meta.dir so the earlier process.chdir() can't break resolution. We pin the binary
// here rather than tracking @gitlawb/openclaude (it's drifting toward a coding-agent product + likely
// to be dropped). See vendor/openclaude-0.18.0/README.md. Penalty passthrough is NOT patched (needs a
// source build) — deferred to the own-bridge.
const { query } = await import(`${import.meta.dir}/vendor/openclaude-0.18.0/sdk.mjs`)

const emit = (obj: unknown) => process.stdout.write(JSON.stringify(obj) + '\n')

// MCP **capability** servers are now operator config (the `mcp_servers` registry), assembled
// per-state by the Rust harness — which resolves each server's auth token into its http header
// or stdio env so the token never reaches the agent — and passed in the `invoke` message. This
// bridge stays generic: adding cove.trade or the next tool is a config entry, no code change here.
// (`openclaude-bridge/twitter-mcp.ts` is the duck's own stdio capability server the registry points at.)

const OUTPUT_INSTRUCTION =
  'When you have finished perceiving and taking any permitted actions, your FINAL message ' +
  'MUST be ONLY a single JSON object (no prose, no markdown fence) with this shape: ' +
  '{"thought": string, ' +
  '"tag_notes": [{"tag": string, "note": string}]|null, ' +
  '"spawn": {"agent": string, "brief": string}|null, ' +
  '"batons": [{"to_prompt": string, "gist": string, "priority": "low"|"normal"|"high"|"urgent"|null}]}. ' +
  '"batons" is your fan-out: ONE element to take a single next step, SEVERAL to do several things at ' +
  'once (each its OWN gist + destination, each gated independently by your trust ceiling), or [] to ' +
  'stop. Each "to_prompt" must be one of the ids in your allowed-transitions context block. Set ' +
  '"spawn" to delegate a job to a worker (only if your orientation lists it as available; the worker ' +
  'runs detached and returns later), else null. Output nothing after the JSON.'

// PARSE-FAIL recovery: how many times to re-prompt for the JSON when the model emits prose instead.
// Bounded here AND by the harness's `invoke_timeout_secs` (the whole round-trip shares that budget).
const MAX_PARSE_RETRIES = 2
// The recovery turn's prompt. It runs in the SAME session with EVERY tool denied (see runInvoke), so
// it physically cannot re-fire an action the failed turn may have already taken — it can only emit text.
const RETRY_PROMPT =
  'Your previous message was not the required JSON object, so it could not be processed. Do NOT call ' +
  'any tool or take any further action — anything you intended is already done or no longer needed. ' +
  'Reply with ONLY the single JSON object (no prose, no markdown fence) describing your decision, in ' +
  'the shape you were given. If nothing further is needed, return it with "batons": [].'

const pending = new Map<string, (d: { allow: boolean; message?: string }) => void>()
let started = false

const rl = readline.createInterface({ input: process.stdin })
rl.on('line', (line: string) => {
  const t = line.trim()
  if (!t) return
  let msg: any
  try { msg = JSON.parse(t) } catch { return }
  if (msg.kind === 'invoke' && !started) {
    started = true
    runInvoke(msg).catch((e) => {
      emit({ kind: 'error', message: String(e?.message ?? e) })
      process.exit(1)
    })
  } else if (msg.kind === 'decision') {
    const resolve = pending.get(msg.tool_use_id)
    if (resolve) { pending.delete(msg.tool_use_id); resolve(msg) }
  }
})

async function runInvoke(inv: any) {
  const options: any = {
    // The agent operates in the soul repo (so its file tools reach memory/, skills/, …);
    // falls back to the bridge's cwd for pure-text runs.
    cwd: inv.cwd ?? process.cwd(),
    systemPrompt: { type: 'custom', content: `${inv.system_prompt}\n\n${OUTPUT_INSTRUCTION}` },
    // No sub-agent defs (`inv.agents`) ⇒ disallow the sub-agent tool (`Task`/`Agent`). The SDK
    // Docker-sandboxes each sub-agent; inside a DOCKER-isolated worker there is no docker daemon
    // (no docker-in-docker, by design), so a sub-agent spawn dies with "failed to connect to the
    // docker API" and takes the bridge with it. A docker worker is a SINGLE isolated agent; the duck
    // (also no agents) never uses Task either. A HOST worker passes agents → these stay allowed.
    disallowedTools: [
      ...(inv.disallowed_tools ?? []),
      // The SDK's Docker-sandboxed bash variant: it connects to the docker daemon, which doesn't exist
      // inside a DACK-isolated worker container (no docker-in-docker) — disallow it so the model uses
      // plain `Bash` (the harness container + the wall ARE the sandbox).
      'SandboxedBash',
      ...(inv.agents && Object.keys(inv.agents).length ? [] : ['Task', 'Agent']),
    ],
    // The wall: relay every tool to Rust and block on its decision.
    canUseTool: async (name: string, input: unknown, opts?: { toolUseID?: string }) => {
      const tool_use_id = opts?.toolUseID ?? globalThis.crypto.randomUUID()
      emit({ kind: 'permission', tool: name, tool_use_id, input })
      const decision = await new Promise<{ allow: boolean; message?: string }>((resolve) =>
        pending.set(tool_use_id, resolve),
      )
      if (!decision.allow) {
        return { behavior: 'deny', message: decision.message ?? 'denied by DACK wall' }
      }
      // Disable the SDK's OWN bash sandbox (which Docker-sandboxes some commands) — it dies inside a
      // DACK-isolated worker container (no docker-in-docker). The wall already gated this call, and
      // the harness container IS the sandbox, so the SDK's redundant one is both unwanted and broken.
      const updatedInput =
        name === 'Bash' || name === 'SandboxedBash'
          ? { ...(input as any), dangerouslyDisableSandbox: true }
          : (input as any)
      return { behavior: 'allow', updatedInput }
    },
  }
  if (inv.model) options.model = inv.model
  if (inv.allowed_tools) options.allowedTools = inv.allowed_tools
  // Sticky-session resume (the harness passes the session_id it wants to continue). Absent = fresh.
  if (inv.resume) options.resume = inv.resume
  // The duck's act-phase capabilities, assembled by the harness for this state (tokens injected
  // into headers/env). The wall still gates EVERY call (canUseTool → Rust), tier-classified.
  if (inv.mcp_servers && typeof inv.mcp_servers === 'object') options.mcpServers = inv.mcp_servers
  // Sub-agent defs the worker may Task-spawn. Empty for the duck's states.
  if (inv.agents && typeof inv.agents === 'object' && Object.keys(inv.agents).length) options.agents = inv.agents

  // First pass: the real invocation — tools live, the wall (canUseTool → Rust) gating every call.
  let { finalText, sessionId, usage } = await runQuery(inv.user_prompt, options, inv.model)
  let parsed = tryParseOutput(finalText, console.error)

  // Bounded PARSE-FAIL recovery: ONLY when the model emitted prose instead of the closing JSON
  // (`ok: false` — a valid `{"batons":[]}` stop parses fine and is skipped). Re-prompt IN THE SAME
  // SESSION for just the JSON, with EVERY tool denied, so a recovery turn can never re-fire an action
  // the first turn may have already taken (no duplicate post/reply/trade). Bounded by MAX_PARSE_RETRIES
  // and, overall, by the harness's invoke timeout.
  let attempt = 0
  while (!parsed.ok && attempt < MAX_PARSE_RETRIES && sessionId) {
    attempt++
    console.error(
      `[bridge:parse] PARSE-FAIL recovery ${attempt}/${MAX_PARSE_RETRIES}: re-prompting for JSON only (all tools denied)`,
    )
    const retryOptions: any = {
      ...options,
      resume: sessionId,
      // No capability servers for a text-only turn — nothing to reconnect.
      mcpServers: undefined,
      // Hard firewall: deny EVERY tool on the recovery turn → it can only produce text, never an action.
      canUseTool: async () => ({ behavior: 'deny', message: 'recovery turn: no tools — reply with ONLY the JSON.' }),
    }
    const r = await runQuery(RETRY_PROMPT, retryOptions, inv.model)
    if (r.sessionId) sessionId = r.sessionId
    if (r.usage) usage = r.usage
    finalText = r.finalText
    parsed = tryParseOutput(finalText, console.error)
  }

  // `usage` (the resumed-context token counts) rides the result so the harness can size-evict a session.
  emit({ kind: 'result', output: parsed.output, session_id: sessionId, usage })
  process.exit(0)
}

/** Drive one model turn to completion: accumulate its final text, session id, and usage. */
async function runQuery(
  prompt: string,
  options: any,
  model: string | null,
): Promise<{ finalText: string; sessionId: string | null; usage: any }> {
  let finalText = ''
  let sessionId: string | null = null
  let usage: any = null
  for await (const m of query({ prompt, options }) as AsyncIterable<any>) {
    // Capture the engine session id (carried on the SDK messages) → returned for sticky resume.
    if (m?.session_id) sessionId = m.session_id
    // Concise capability connection status (NOT the tool list — keep it grep-safe): an operator
    // sees at a glance whether cove/twitter connected or failed.
    if (m?.type === 'system' && m?.subtype === 'init' && Array.isArray(m.mcp_servers) && m.mcp_servers.length) {
      console.error('[bridge:mcp]', JSON.stringify(m.mcp_servers.map((s: any) => `${s.name}:${s.status}`)))
    }
    if (m?.type === 'assistant' && Array.isArray(m.message?.content)) {
      for (const b of m.message.content) if (b.type === 'text') finalText += b.text
    } else if (m?.type === 'result') {
      if (m?.result) finalText ||= m.result
      usage = m.usage ?? m.modelUsage ?? null
      // Cost telemetry → stderr (inherited by the harness log). Per-invocation = per-state. `subtype`
      // tells us WHY a turn ended (end_turn vs an interrupt vs max-turns) — the signal for diagnosing
      // a model that calls a tool then skips the closing JSON.
      console.error(
        '[bridge:usage]',
        JSON.stringify({
          model: model ?? null,
          subtype: m.subtype ?? null,
          usage,
          cost_usd: m.total_cost_usd ?? null,
          duration_ms: m.duration_ms ?? null,
          num_turns: m.num_turns ?? null,
        }),
      )
    }
  }
  return { finalText, sessionId, usage }
}
