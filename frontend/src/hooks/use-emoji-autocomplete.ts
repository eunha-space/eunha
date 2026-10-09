import {
  useCallback,
  useMemo,
  useState,
  type Dispatch,
  type KeyboardEvent,
  type RefObject,
  type SetStateAction,
} from 'react'

import type { mastodon } from '../masto.ts'
import { searchCustomEmojis, useCustomEmojis } from './use-custom-emojis.ts'

// The `:shortcode` being typed just left of the caret. Mastodon's composer
// suggests once the token, colon included, is three characters long, and only
// when the colon starts a word of its own (`textAtCursorMatchesToken`).
function activeShortcode(text: string, caret: number): { start: number; query: string } | null {
  const m = /(?:^|[^\w:@#＠＃+-]):([\w+-]{2,})$/.exec(text.slice(0, caret))
  if (!m) return null
  return { start: caret - m[1].length - 1, query: m[1] }
}

/**
 * Custom emoji suggestions for a textarea, as Mastodon's composer offers them
 * on `:`: up to five of the server's emoji, best match first. Picking one
 * replaces the typed token with `:shortcode: `.
 */
export function useEmojiAutocomplete({
  enabled,
  text,
  setText,
  caret,
  setCaret,
  textareaRef,
}: {
  enabled: boolean
  text: string
  setText: Dispatch<SetStateAction<string>>
  caret: number
  setCaret: Dispatch<SetStateAction<number>>
  textareaRef: RefObject<HTMLTextAreaElement | null>
}) {
  const emojis = useCustomEmojis(enabled)
  const token = enabled ? activeShortcode(text, caret) : null
  const query = token?.query ?? null
  const suggestions = useMemo(
    () => (query ? searchCustomEmojis(emojis, query, 5) : []),
    [emojis, query],
  )

  const [active, setActive] = useState(0)
  const [dismissed, setDismissed] = useState<string | null>(null)
  // A new query starts at the top again, and undoes an Escape.
  const [queryBefore, setQueryBefore] = useState(query)
  if (queryBefore !== query) {
    setQueryBefore(query)
    setActive(0)
    setDismissed(null)
  }

  const open = token != null && dismissed !== query && suggestions.length > 0

  const select = useCallback(
    (emoji: mastodon.v1.CustomEmoji) => {
      if (!token) return
      const insert = `:${emoji.shortcode}: `
      const next = text.slice(0, token.start) + insert + text.slice(caret)
      const nextCaret = token.start + insert.length
      setText(next)
      setCaret(nextCaret)
      requestAnimationFrame(() => {
        const el = textareaRef.current
        if (el) {
          el.focus()
          el.setSelectionRange(nextCaret, nextCaret)
        }
      })
    },
    [token, text, caret, setText, setCaret, textareaRef],
  )

  const onKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (!open) return
      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault()
          setActive((a) => (a + 1) % suggestions.length)
          break
        case 'ArrowUp':
          e.preventDefault()
          setActive((a) => (a - 1 + suggestions.length) % suggestions.length)
          break
        case 'Enter':
        case 'Tab':
          e.preventDefault()
          select(suggestions[Math.min(active, suggestions.length - 1)])
          break
        case 'Escape':
          e.preventDefault()
          setDismissed(query)
          break
      }
    },
    [open, suggestions, active, select, query],
  )

  return { open, suggestions, active, setActive, select, onKeyDown }
}
