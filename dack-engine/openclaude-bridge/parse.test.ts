import { test, expect } from 'bun:test'
import { parseOutput, tryParseOutput, extractJsonObjects } from './parse'

test('single clean object parses unchanged (the normal path)', () => {
  const out: any = parseOutput('{"thought":"t","transition":{"to_prompt":"express","reason":""}}')
  expect(out.thought).toBe('t')
  expect(out.transition.to_prompt).toBe('express')
})

test('multi-object (the live bitconnect drop) keeps the real transition via later-wins merge', () => {
  // obj1 = gist only (NO transition); obj2 = the complete decision WITH the transition. Before the
  // fix this fell back to to_prompt=null → the Telegram reply silently never sent.
  const text =
    '{"thought":"vibe","tag_notes":null,"proposal":{"intent":"reply","gist":"BITCONNEEEECT"}},' +
    '{"thought":"vibe2","proposal":{"intent":"reply","gist":"BITCONNEEEECT"},"spawn":null,' +
    '"transition":{"to_prompt":"telegram/express","reason":"match energy"}}'
  const logs: string[] = []
  const out: any = parseOutput(text, (m) => logs.push(m))
  expect(out.transition.to_prompt).toBe('telegram/express')
  expect(logs.join(' ')).toContain('multi-object')
})

test('NDJSON (newline-separated) objects are both extracted', () => {
  expect(extractJsonObjects('{"a":1}\n{"b":2}').length).toBe(2)
})

test('array-wrapped objects are extracted', () => {
  expect(extractJsonObjects('[{"a":1},{"b":2}]').length).toBe(2)
})

test('braces inside strings do not break extraction', () => {
  const objs = extractJsonObjects('{"gist":"a } b { c","transition":{"to_prompt":"x"}}')
  expect(objs.length).toBe(1)
  expect(objs[0].transition.to_prompt).toBe('x')
})

test('fenced single object still parses', () => {
  const out: any = parseOutput('```json\n{"thought":"f","transition":{"to_prompt":null}}\n```')
  expect(out.thought).toBe('f')
})

test('multi-object batons are CONCATENATED, not dropped (later-wins would lose some)', () => {
  // The model split its fan-out across two JSON objects — both batons must survive.
  const text =
    '{"thought":"a","batons":[{"to_prompt":"telegram/express","reply_to":"10","gist":"A"}]}\n' +
    '{"thought":"b","batons":[{"to_prompt":"telegram/express","reply_to":"20","gist":"B"}]}'
  const out: any = parseOutput(text, () => {})
  expect(out.batons.length).toBe(2)
  expect(out.batons.map((b: any) => b.reply_to)).toEqual(['10', '20'])
})

test('EXACT-duplicate batons are dropped, distinct ones kept (repetition artifact filter)', () => {
  const logs: string[] = []
  const out: any = parseOutput(
    '{"thought":"t","batons":[' +
      '{"to_prompt":"telegram/express","gist":"reply A"},' +
      '{"to_prompt":"telegram/express","gist":"reply A"},' + // exact dup → dropped
      '{"to_prompt":"telegram/express","gist":"reply B"}]}', // distinct → kept
    (m) => logs.push(m),
  )
  expect(out.batons.map((b: any) => b.gist)).toEqual(['reply A', 'reply B'])
  expect(logs.join(' ')).toContain('dropped 1 exact-duplicate')
})

test('the live 4-object verbatim case collapses to the distinct batons (2 pairs → 2)', () => {
  // The model re-emitted its decision JSON 4× → 2 verbatim pairs (what the runlog showed). Merge
  // concatenates, then exact-dedup collapses each pair — the two DISTINCT wordings survive.
  const text =
    '{"thought":"x","batons":[{"to_prompt":"telegram/express","gist":"A"}]}\n' +
    '{"thought":"x","batons":[{"to_prompt":"telegram/express","gist":"A"}]}\n' +
    '{"thought":"x","batons":[{"to_prompt":"telegram/express","gist":"B"}]}\n' +
    '{"thought":"x","batons":[{"to_prompt":"telegram/express","gist":"B"}]}'
  const out: any = parseOutput(text)
  expect(out.batons.map((b: any) => b.gist)).toEqual(['A', 'B'])
})

test('a NUMERIC baton reply_to is coerced to a string (Rust expects Option<String>)', () => {
  // The model emits the message_id as a JSON number; an integer would crash the Rust deserialize.
  const out: any = parseOutput(
    '{"thought":"t","batons":[{"to_prompt":"telegram/express","reply_to":273,"gist":"reply"}]}',
  )
  expect(out.batons[0].reply_to).toBe('273')
  expect(typeof out.batons[0].reply_to).toBe('string')
})

test('total garbage logs loudly and terminates (to_prompt null), never silently', () => {
  const logs: string[] = []
  const out: any = parseOutput('the model rambled with no json at all', (m) => logs.push(m))
  expect(out.transition.to_prompt).toBe(null)
  expect(out.thought).toContain('rambled')
  expect(logs.join(' ')).toContain('PARSE-FAIL')
})

// --- the parse-status signal that gates PARSE-FAIL recovery (the bridge only re-prompts on ok:false) ---

test('tryParseOutput: prose with no JSON → ok:false (triggers recovery)', () => {
  const r = tryParseOutput('Good piece. Honest take: the holacracy framing is clean...')
  expect(r.ok).toBe(false)
  expect((r.output as any).thought).toContain('Good piece')
})

test('tryParseOutput: a valid empty-baton STOP → ok:true (must NOT trigger recovery)', () => {
  // The model deliberately chose to stop. This is valid JSON — re-prompting it would be wrong.
  const r = tryParseOutput('{"thought":"nothing to do","batons":[]}')
  expect(r.ok).toBe(true)
  expect((r.output as any).batons).toEqual([])
})

test('tryParseOutput: a normal single object → ok:true', () => {
  const r = tryParseOutput('{"thought":"t","batons":[{"to_prompt":"telegram/express","gist":"g"}]}')
  expect(r.ok).toBe(true)
})

test('tryParseOutput: multi-object (recovered) → ok:true (no further recovery needed)', () => {
  const r = tryParseOutput('{"thought":"a"}\n{"thought":"b","transition":{"to_prompt":"express"}}')
  expect(r.ok).toBe(true)
})

test('tryParseOutput: fenced JSON → ok:true', () => {
  const r = tryParseOutput('```json\n{"thought":"f","batons":[]}\n```')
  expect(r.ok).toBe(true)
})
