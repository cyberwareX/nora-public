/**
 * Structured-output parsing for the DACK ↔ OpenClaude bridge.
 *
 * The model's FINAL message is meant to be ONE JSON object matching the Rust `AgentOutput`. In
 * practice a model occasionally emits SEVERAL objects — NDJSON, a JSON array, or a duplicated
 * decision (the model "writes it twice", the second copy complete). The old single `JSON.parse`
 * dropped those: it fell back to a null transition, a SILENT cycle-terminate (observed live as a
 * Telegram reply that never sent). This module is tolerant — it pulls every top-level object out,
 * and when there are several it merges them later-wins so the real `transition` survives, logging
 * it loudly. Acting on each object as its own intent-baton is this phase only stops the
 * silent drop.
 */

/** Ensure the parsed/fallback output satisfies the Rust `AgentOutput` contract. `log` receives a
 * diagnostic if any exact-duplicate batons are dropped (the bridge passes `console.error`). */
export function normalize(o: any, fallbackText: string, log: (m: string) => void = () => {}): unknown {
  const out = o && typeof o === 'object' ? { ...o } : {}
  if (typeof out.thought !== 'string') out.thought = fallbackText.trim().slice(0, 2000) || '(no output)'
  if (!out.transition || typeof out.transition !== 'object') out.transition = { to_prompt: null }
  // The model is told to set a baton's `reply_to` to a `message_id`, which is naturally NUMERIC — so it
  // often emits `reply_to: 273` (a JSON number). Rust's `BatonIntent.reply_to` is `Option<String>` and
  // serde rejects an integer ("invalid type: integer 273, expected a string") → the whole cycle fails to
  // parse. Coerce id-like baton fields to strings so a numeric id can't kill a dispatch.
  if (Array.isArray(out.batons)) {
    for (const b of out.batons) {
      if (b && typeof b.reply_to === 'number') b.reply_to = String(b.reply_to)
    }
    // Drop EXACT-duplicate batons (byte-identical after the coercion above). A verbatim repeat is a
    // known neural-text-degeneration artifact (the model re-emits the same object) — NOT a distinct
    // intent, which is never byte-identical. DISTINCT/paraphrased batons are untouched, so genuine
    // multi-replies survive. Logged so the rate stays visible.
    const seen = new Set<string>()
    const deduped = out.batons.filter((b: any) => {
      const key = JSON.stringify(b)
      if (seen.has(key)) return false
      seen.add(key)
      return true
    })
    if (deduped.length < out.batons.length) {
      log(`[bridge:parse] dropped ${out.batons.length - deduped.length} exact-duplicate baton(s) (repetition artifact)`)
    }
    out.batons = deduped
  }
  return out
}

/**
 * Every balanced top-level `{...}` object in `s`, parsed. Braces inside strings are ignored (so a
 * `}` in a gist can't end an object early), and unparseable chunks are skipped. Robust to NDJSON,
 * comma-joined objects, array wrappers, and surrounding prose.
 */
export function extractJsonObjects(s: string): any[] {
  const objs: any[] = []
  let depth = 0
  let start = -1
  let inStr = false
  let esc = false
  for (let i = 0; i < s.length; i++) {
    const c = s[i]
    if (inStr) {
      if (esc) esc = false
      else if (c === '\\') esc = true
      else if (c === '"') inStr = false
      continue
    }
    if (c === '"') inStr = true
    else if (c === '{') {
      if (depth === 0) start = i
      depth++
    } else if (c === '}' && depth > 0) {
      if (--depth === 0 && start >= 0) {
        try {
          objs.push(JSON.parse(s.slice(start, i + 1)))
        } catch {
          /* skip an unbalanced/garbage chunk */
        }
        start = -1
      }
    }
  }
  return objs
}

/**
 * Extract the `AgentOutput` from the model's final text (tolerant of fences / prose / multi-object),
 * AND report whether a JSON object was actually found. `ok: false` ONLY on a genuine PARSE-FAIL (the
 * model emitted prose, not the contract) — a valid stop (`{"batons":[]}`) parses fine and is
 * `ok: true`. The bridge uses `ok` to decide whether to re-prompt for the JSON. `log` receives any
 * loud diagnostics (the bridge passes `console.error`; tests capture them).
 */
export function tryParseOutput(
  text: string,
  log: (m: string) => void = () => {},
): { ok: boolean; output: unknown } {
  let t = text.trim()
  const fence = t.match(/```(?:json)?\s*([\s\S]*?)```/i)
  if (fence && fence[1]) t = fence[1].trim()

  // Fast path: a single clean object (the normal case — behaviour unchanged).
  try {
    return { ok: true, output: normalize(JSON.parse(t), text, log) }
  } catch {
    /* fall through to the robust scan */
  }

  // Robust path: every top-level object the model emitted.
  const objs = extractJsonObjects(t)
  if (objs.length === 1) return { ok: true, output: normalize(objs[0], text, log) }
  if (objs.length > 1) {
    // Merge later-wins for scalar fields (thought/transition/spawn), but CONCATENATE the `batons`
    // arrays across objects — the model sometimes splits its fan-out across several JSON objects, and
    // a plain later-wins merge would silently DROP the earlier objects' batons. `normalize` then drops
    // any EXACT-duplicate batons (the verbatim "writes it N times" degeneration artifact).
    const merged: any = Object.assign({}, ...objs)
    const allBatons = objs.flatMap((o: any) => (Array.isArray(o.batons) ? o.batons : []))
    if (allBatons.length) merged.batons = allBatons
    log(
      `[bridge:parse] multi-object output: ${objs.length} JSON objects — merged ` +
        `(${allBatons.length} batons concatenated)`,
    )
    return { ok: true, output: normalize(merged, text, log) }
  }

  // Nothing parseable: keep the raw text in `thought` and terminate — but LOUD, never silent.
  log(
    `[bridge:parse] PARSE-FAIL: no parseable JSON object in the model's ${text.length}-char final ` +
      `message — cycle terminates (to_prompt=null); raw kept in thought`,
  )
  return { ok: false, output: normalize(null, text) }
}

/** Backward-compatible wrapper: just the normalized output (callers that don't need the parse status). */
export function parseOutput(text: string, log: (m: string) => void = () => {}): unknown {
  return tryParseOutput(text, log).output
}
