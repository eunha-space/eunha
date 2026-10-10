import { useEffect, useState } from 'react'

import { loadUnicodeEmojis, loadedUnicodeEmojis, type UnicodeEmoji } from '@/lib/emoji-search.ts'

/**
 * The Unicode emoji, fetched the first time something enabled asks for them
 * and shared from then on; null until they arrive.
 */
export function useUnicodeEmojis(enabled = true): UnicodeEmoji[] | null {
  const [emojis, setEmojis] = useState<UnicodeEmoji[] | null>(loadedUnicodeEmojis)
  useEffect(() => {
    if (!enabled || emojis) return
    let live = true
    void loadUnicodeEmojis().then((list) => {
      if (live) setEmojis(list)
    })
    return () => {
      live = false
    }
  }, [enabled, emojis])
  return loadedUnicodeEmojis() ?? emojis
}
