// The picker's "Frequently used" row, as Mastodon's composer keeps it: a count
// for each emoji picked, from the picker or the `:` suggestions, the sixteen
// most used first, topped up with Mastodon's defaults. Mastodon keeps the
// counts in the account's web settings; eunha keeps them in this browser, per
// account.
import { getActiveAccountId } from '../auth.ts'

// Mastodon's `perLine * lines`.
const SHOWN = 16

// Mastodon's `DEFAULTS`, by their iamcal shortcodes.
export const DEFAULT_FREQUENT = [
  '+1',
  'grinning',
  'kissing_heart',
  'heart_eyes',
  'laughing',
  'stuck_out_tongue_winking_eye',
  'sweat_smile',
  'joy',
  'yum',
  'disappointed',
  'thinking_face',
  'weary',
  'sob',
  'sunglasses',
  'heart',
  'ok_hand',
]

// A Unicode emoji is counted by its character, a custom one by `:shortcode:`.
export type EmojiKey = string

const storageKey = () => `eunha:frequent-emoji:${getActiveAccountId() ?? 'anonymous'}`

function readCounts(): Record<EmojiKey, number> {
  try {
    const raw = localStorage.getItem(storageKey())
    const parsed: unknown = raw ? JSON.parse(raw) : {}
    return parsed && typeof parsed === 'object' ? (parsed as Record<EmojiKey, number>) : {}
  } catch {
    return {}
  }
}

/** Mastodon's `useEmoji`: one more use of this emoji. */
export function recordEmojiUse(key: EmojiKey) {
  const counts = readCounts()
  counts[key] = (counts[key] ?? 0) + 1
  try {
    localStorage.setItem(storageKey(), JSON.stringify(counts))
  } catch {
    // A private window keeps no counts; the defaults still show.
  }
}

/**
 * The most used first, at most sixteen. Fewer than sixteen leaves room for
 * the defaults, which `resolveDefault` turns into keys.
 */
export function frequentEmojiKeys(resolveDefault: (shortcode: string) => EmojiKey | null): EmojiKey[] {
  const counts = readCounts()
  const keys = Object.keys(counts)
    .sort((a, b) => counts[b] - counts[a])
    .slice(0, SHOWN)
  if (keys.length < DEFAULT_FREQUENT.length) {
    const defaults = DEFAULT_FREQUENT.map(resolveDefault).filter(
      (key): key is EmojiKey => !!key && !keys.includes(key),
    )
    keys.push(...defaults.slice(0, DEFAULT_FREQUENT.length - keys.length))
  }
  return keys
}
