import { useEffect, useState } from 'react'

import type { mastodon } from '../masto.ts'
import { getCustomEmojis } from '../api.ts'

// Asked for once per page load and shared, as Mastodon's client fetches them
// once at start-up.
let request: Promise<mastodon.v1.CustomEmoji[]> | null = null
let loaded: mastodon.v1.CustomEmoji[] | null = null

function load(): Promise<mastodon.v1.CustomEmoji[]> {
  request ??= getCustomEmojis()
    .then((emojis) => {
      loaded = emojis.filter((e) => e.visibleInPicker !== false)
      return loaded
    })
    .catch(() => {
      // Asked again by the next composer, rather than never.
      request = null
      return []
    })
  return request
}

/** The server's custom emoji offered in the picker; empty until they load. */
export function useCustomEmojis(enabled = true): mastodon.v1.CustomEmoji[] {
  const [emojis, setEmojis] = useState<mastodon.v1.CustomEmoji[]>(loaded ?? [])
  useEffect(() => {
    if (!enabled || loaded) return
    let live = true
    void load().then((list) => {
      if (live) setEmojis(list)
    })
    return () => {
      live = false
    }
  }, [enabled])
  return loaded ?? emojis
}

/**
 * The emoji whose shortcode holds `query`, best first: an exact match, then
 * those that start with it, then the rest — the order Mastodon's emoji search
 * scores a shortcode in.
 */
export function searchCustomEmojis(
  emojis: readonly mastodon.v1.CustomEmoji[],
  query: string,
  limit = Infinity,
): mastodon.v1.CustomEmoji[] {
  const q = query.toLowerCase()
  const rank = (code: string) => (code === q ? 0 : code.startsWith(q) ? 1 : 2)
  return emojis
    .filter((e) => e.shortcode.toLowerCase().includes(q))
    .sort((a, b) => {
      const ra = rank(a.shortcode.toLowerCase())
      const rb = rank(b.shortcode.toLowerCase())
      return ra - rb || a.shortcode.localeCompare(b.shortcode)
    })
    .slice(0, limit)
}

// Mastodon's `CHARS_ALLOWED_AROUND_EMOJI`: a shortcode needs a space before
// it unless one of these already sits there.
// eslint-disable-next-line no-control-regex
const ALLOWED_BEFORE = /[>< \u2026\u0009-\u000d\u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]/

/**
 * Mastodon's `insertEmojiAtPosition`: `:code:` and a space at `position`, with
 * a space before it where the text before would run into it.
 */
export function insertShortcode(text: string, shortcode: string, position: number) {
  const needsSpace = position > 0 && !ALLOWED_BEFORE.test(text[position - 1] ?? '')
  const insert = `${needsSpace ? ' ' : ''}:${shortcode}: `
  return {
    text: `${text.slice(0, position)}${insert}${text.slice(position)}`,
    caret: position + insert.length,
  }
}
