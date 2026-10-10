// Unicode emoji, and the search the composer's `:` suggestions and the emoji
// picker run over them and the server's custom emoji together.
//
// The data is Mastodon's own: `emojibase-data`'s English compact set, joined
// with its CLDR shortcodes and the iamcal ones Mastodon keeps as its legacy
// shortcodes. It is a sizable file, so it is fetched as a chunk of its own the
// first time a picker or a suggestion needs it.
import type { CompactEmoji, ShortcodesDataset } from 'emojibase'

import type { mastodon } from '../masto.ts'

export interface UnicodeEmoji {
  hexcode: string
  unicode: string
  // What a pick inserts or reacts with: the form Mastodon's picker and its
  // reaction validator know. Emojibase writes a variation selector after an
  // emoji that has a text form; where the emoji already shows as an emoji by
  // default it is dropped, as in Mastodon's emoji map.
  native: string
  label: string
  // What Mastodon's suggestions call it: the label, lowercased, with
  // underscores for spaces (`emojiMartSearch`).
  id: string
  group: number
  order: number
  shortcodes: string[]
  emoticons: string[]
  tokens: string[]
}

export type AnyEmoji =
  | { type: 'unicode'; emoji: UnicodeEmoji }
  | { type: 'custom'; emoji: mastodon.v1.CustomEmoji }

// Mastodon's `EMOJI_MIN_TOKEN_LENGTH`.
const MIN_TOKEN_LENGTH = 2

const segmenter =
  typeof Intl !== 'undefined' && 'Segmenter' in Intl
    ? new Intl.Segmenter('en', { granularity: 'word' })
    : null

/** Mastodon's `extractTokens`: the words of a label, query or shortcode. */
export function extractTokens(input: string): string[] {
  if (!input.trim()) return []
  if (input === '+1' || input === '-1') return [input]
  const tokens: string[] = []
  if (segmenter) {
    const text = input.replaceAll(/[_-]+/g, ' ').replaceAll(/([a-z])([A-Z])/g, '$1 $2')
    for (const { isWordLike, segment } of segmenter.segment(text)) {
      if (isWordLike && segment.length >= MIN_TOKEN_LENGTH) tokens.push(segment.toLowerCase())
    }
  } else {
    for (const word of input.split(/[\s_-]+/)) {
      if (/\w/.test(word) && word.length >= MIN_TOKEN_LENGTH) tokens.push(word.toLowerCase())
    }
  }
  return tokens
}

function toUnicodeEmoji(
  emoji: CompactEmoji,
  cldr: ShortcodesDataset,
  iamcal: ShortcodesDataset,
): UnicodeEmoji {
  const list = (value: string | string[] | undefined) =>
    value === undefined ? [] : Array.isArray(value) ? value : [value]
  const own = list(cldr[emoji.hexcode])
  const emoticons = list(emoji.emoticon)
  const bare = String.fromCodePoint(...emoji.hexcode.split('-').map((h) => parseInt(h, 16)))
  return {
    hexcode: emoji.hexcode,
    unicode: emoji.unicode,
    native: /^\p{Emoji_Presentation}/u.test(emoji.unicode) ? bare : emoji.unicode,
    label: emoji.label,
    id: emoji.label.replaceAll(' ', '_').toLowerCase(),
    group: emoji.group ?? -1,
    order: emoji.order ?? 0,
    shortcodes: [...new Set([...own, ...list(iamcal[emoji.hexcode])])],
    emoticons,
    // Mastodon's `transformEmojiData`.
    tokens: [
      ...new Set([
        ...own.flatMap(extractTokens),
        ...(emoji.tags ?? []).flatMap(extractTokens),
        ...extractTokens(emoji.label),
        ...emoticons,
      ]),
    ],
  }
}

let request: Promise<UnicodeEmoji[]> | null = null
let loaded: UnicodeEmoji[] | null = null

/**
 * Every Unicode emoji the picker offers, in Unicode's order. The skin tone
 * swatches (the `component` group) and the bare regional indicators, which
 * have no group, are not offered, as Mastodon's picker does not offer them.
 */
export function loadUnicodeEmojis(): Promise<UnicodeEmoji[]> {
  request ??= Promise.all([
    import('emojibase-data/en/compact.json'),
    import('emojibase-data/en/shortcodes/cldr.json'),
    import('emojibase-data/en/shortcodes/iamcal.json'),
  ])
    .then(([compact, cldr, iamcal]) => {
      loaded = compact.default
        .filter((e) => e.group !== undefined && e.group !== 2)
        .sort((a, b) => (a.order ?? 0) - (b.order ?? 0))
        .map((e) => toUnicodeEmoji(e, cldr.default, iamcal.default))
      return loaded
    })
    .catch(() => {
      request = null
      return []
    })
  return request
}

export function loadedUnicodeEmojis(): UnicodeEmoji[] | null {
  return loaded
}

// The fields Mastodon's search scores, in the order it weighs them; the first
// four name the emoji, `tokens` only describe it.
const FIELDS = ['label', 'shortcode', 'emoticons', 'shortcodes', 'tokens'] as const
type Field = (typeof FIELDS)[number]
type Scores = Record<Field, number>
const IDENTIFIERS = new Set<Field>(['label', 'shortcode', 'emoticons', 'shortcodes'])

function fieldValues(emoji: AnyEmoji, field: Field): string[] {
  if (emoji.type === 'custom') {
    const code = emoji.emoji.shortcode
    if (field === 'shortcode') return [code]
    if (field === 'tokens') {
      const tokens = extractTokens(code)
      return tokens.includes(code) ? tokens : [code, ...tokens]
    }
    return []
  }
  const e = emoji.emoji
  switch (field) {
    case 'label':
      return [e.label]
    case 'shortcode':
      return []
    default:
      return e[field]
  }
}

// Mastodon's `getScoreForEmojiTokens`: exact, then prefix, then substring,
// the closer the match in length the better.
function scoreValues(values: string[], query: string): number {
  let lowest = -1
  for (const raw of values) {
    const value = raw.toLowerCase()
    let score = -1
    if (value === query) score = 0
    else if (value.startsWith(query)) score = 1 + query.length / value.length
    else if (value.includes(query)) score = 2 + query.length / value.length
    if (score >= 0 && (score < lowest || lowest < 0)) lowest = score
  }
  return lowest
}

function scoreEmoji(emoji: AnyEmoji, query: string): Scores | null {
  const scores = Object.fromEntries(FIELDS.map((f) => [f, -1])) as Scores
  let any = false
  for (const field of FIELDS) {
    const score = scoreValues(fieldValues(emoji, field), query)
    if (score >= 0) {
      scores[field] = score
      any = true
    }
  }
  return any ? scores : null
}

function combine(a: Scores, b: Scores): Scores {
  const scores = {} as Scores
  for (const f of FIELDS) {
    scores[f] = a[f] === -1 ? b[f] : b[f] === -1 ? a[f] : Math.min(a[f], b[f])
  }
  return scores
}

interface Rank {
  categoryWeight: number
  score: number
  fieldWeight: number
}

function bestRank(scores: Scores): Rank {
  let best: Rank | null = null
  FIELDS.forEach((field, fieldWeight) => {
    const score = scores[field]
    if (score < 0) return
    const categoryWeight = IDENTIFIERS.has(field) ? 0 : 1
    if (
      !best ||
      categoryWeight < best.categoryWeight ||
      (categoryWeight === best.categoryWeight &&
        (score < best.score || (score === best.score && fieldWeight < best.fieldWeight)))
    ) {
      best = { categoryWeight, score, fieldWeight }
    }
  })
  return best ?? { categoryWeight: 2, score: Infinity, fieldWeight: FIELDS.length }
}

const keyOf = (emoji: AnyEmoji) =>
  emoji.type === 'custom' ? `:${emoji.emoji.shortcode}` : emoji.emoji.hexcode

/**
 * Mastodon's emoji `search`: each word of the query is matched against every
 * emoji, the emoji that match all of them are kept, and when that leaves fewer
 * than `limit` the custom emoji whose shortcode holds the query are added.
 * Matches on what names an emoji outrank matches on what describes it, then
 * the better match, then the field it was in, and a custom emoji before a
 * Unicode one.
 */
export function searchEmojis(
  unicode: readonly UnicodeEmoji[],
  custom: readonly mastodon.v1.CustomEmoji[],
  rawQuery: string,
  limit = 0,
): AnyEmoji[] {
  const query = rawQuery.toLowerCase().replace(':', '').trim()
  const tokens = extractTokens(query)
  if (tokens.length === 0) return []
  const all: AnyEmoji[] = [
    ...unicode.map((emoji) => ({ type: 'unicode', emoji }) as const),
    ...custom.map((emoji) => ({ type: 'custom', emoji }) as const),
  ]

  let found: Map<string, { emoji: AnyEmoji; scores: Scores }> | null = null
  for (const token of tokens) {
    const next = new Map<string, { emoji: AnyEmoji; scores: Scores }>()
    for (const emoji of all) {
      const key = keyOf(emoji)
      if (found && !found.has(key)) continue
      const scores = scoreEmoji(emoji, token)
      if (!scores) continue
      const before = found?.get(key)
      next.set(key, { emoji, scores: before ? combine(before.scores, scores) : scores })
    }
    found = next
  }
  const results = [...(found ?? new Map()).values()]

  if (results.length < limit || results.length === 0) {
    for (const emoji of custom) {
      const key = `:${emoji.shortcode}`
      if (found?.has(key) || !emoji.shortcode.toLowerCase().includes(query)) continue
      const scores = scoreEmoji({ type: 'custom', emoji }, query)
      if (scores) results.push({ emoji: { type: 'custom', emoji }, scores })
    }
  }

  const order = (e: AnyEmoji) => (e.type === 'unicode' ? e.emoji.order : -1)
  const ranked = results
    .map(({ emoji, scores }) => ({ emoji, rank: bestRank(scores) }))
    .sort(
      (a, b) =>
        a.rank.categoryWeight - b.rank.categoryWeight ||
        a.rank.score - b.rank.score ||
        a.rank.fieldWeight - b.rank.fieldWeight ||
        (a.emoji.type === b.emoji.type ? 0 : a.emoji.type === 'custom' ? -1 : 1) ||
        order(a.emoji) - order(b.emoji),
    )
    .map((r) => r.emoji)
  return limit > 0 ? ranked.slice(0, limit) : ranked
}
