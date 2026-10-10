import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type KeyboardEvent,
  type RefObject,
  type SetStateAction,
} from 'react'

import type { mastodon } from '../masto.ts'
import { searchAccounts } from '../api.ts'

// The `@mention` token immediately to the left of the caret, if any. `start`
// points at the leading `@`; `query` is the handle typed so far (may be empty).
function activeMention(
  text: string,
  caret: number,
): { start: number; query: string } | null {
  const before = text.slice(0, caret)
  // The `@` must sit at the start or after a non-word, non-`@` char so we don't
  // fire inside email-like `foo@bar`. The handle may carry a `@domain` part.
  const m = /(?:^|[^\w@])@([\w.-]*(?:@[\w.-]*)?)$/.exec(before)
  if (!m) return null
  const query = m[1]
  return { start: caret - query.length - 1, query }
}

/**
 * Drives `@mention` autocomplete for a textarea: watches the caret for an
 * in-progress mention, debounces an account search, and returns the suggestion
 * list plus keyboard/selection handlers. The caller owns the textarea value and
 * caret state and renders the dropdown from what this hook returns.
 */
export function useMentionAutocomplete({
  token,
  text,
  setText,
  caret,
  setCaret,
  textareaRef,
}: {
  token: string | null
  text: string
  setText: Dispatch<SetStateAction<string>>
  caret: number
  setCaret: Dispatch<SetStateAction<number>>
  textareaRef: RefObject<HTMLTextAreaElement | null>
}) {
  const mention = token ? activeMention(text, caret) : null
  const query = mention?.query ?? null

  const [suggestions, setSuggestions] = useState<mastodon.v1.Account[]>([])
  const [active, setActive] = useState(0)
  const [dismissed, setDismissed] = useState(false)
  const reqId = useRef(0)

  // Reset the active row and any Escape-dismissal when the query changes.
  const lastQuery = useRef<string | null>(null)
  useEffect(() => {
    if (query !== lastQuery.current) {
      lastQuery.current = query
      setActive(0)
      setDismissed(false)
    }
  }, [query])

  useEffect(() => {
    if (!token || query == null || query.length < 1) {
      setSuggestions([])
      return
    }
    const id = ++reqId.current
    const handle = setTimeout(() => {
      searchAccounts(query, token)
        .then((accounts) => {
          if (reqId.current === id) setSuggestions(accounts)
        })
        .catch(() => {
          if (reqId.current === id) setSuggestions([])
        })
    }, 150)
    return () => clearTimeout(handle)
  }, [token, query])

  const open =
    !dismissed && mention != null && (query?.length ?? 0) >= 1 && suggestions.length > 0

  const select = useCallback(
    (account: mastodon.v1.Account) => {
      if (!mention) return
      const insert = `@${account.acct} `
      const next = text.slice(0, mention.start) + insert + text.slice(caret)
      const nextCaret = mention.start + insert.length
      setText(next)
      setCaret(nextCaret)
      setSuggestions([])
      // Restore focus and drop the caret after the inserted mention.
      requestAnimationFrame(() => {
        const el = textareaRef.current
        if (el) {
          el.focus()
          el.setSelectionRange(nextCaret, nextCaret)
        }
      })
    },
    [mention, text, caret, setText, setCaret, textareaRef],
  )

  const onKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (!open) return
      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault()
          // Mastodon's `AutosuggestTextarea` stops at the last and the first.
          setActive((a) => Math.min(a + 1, suggestions.length - 1))
          break
        case 'ArrowUp':
          e.preventDefault()
          setActive((a) => Math.max(a - 1, 0))
          break
        case 'Enter':
        case 'Tab':
          e.preventDefault()
          select(suggestions[active])
          break
        case 'Escape':
          e.preventDefault()
          setDismissed(true)
          break
      }
    },
    [open, suggestions, active, select],
  )

  return { open, suggestions, active, setActive, select, onKeyDown }
}
