// The link preview the draft would get, for the composer to show while it is
// written. The server picks the link as it will once the post is made, so the
// card shown is the card the post gets; it is kept there, so posting finds it
// rather than fetching the page again.
import { useEffect, useState } from 'react'

import type { mastodon } from '../masto.ts'
import { getLinkCard } from '../eunha-api.ts'

// How long typing has to pause before the draft's links are looked up, so a
// link half typed is not fetched at each keystroke.
const DEBOUNCE_MS = 700

// Mastodon finds a local post's links with `extract_url_without_protocol:
// false`, so only these can give a card. They decide when to ask, and what an
// answer is remembered under; the server still picks among them.
const LINK = /https?:\/\/\S+/gi

const answers = new Map<string, mastodon.v1.PreviewCard | null>()

export function useLinkCard(
  token: string,
  text: string,
  enabled: boolean,
): mastodon.v1.PreviewCard | null {
  const links = enabled ? (text.match(LINK) ?? []).join(' ') : ''
  const [card, setCard] = useState<{ links: string; card: mastodon.v1.PreviewCard | null }>({
    links: '',
    card: null,
  })

  useEffect(() => {
    if (!links || answers.has(links)) return
    const controller = new AbortController()
    const timer = window.setTimeout(() => {
      getLinkCard(token, text, controller.signal)
        .then((found) => {
          answers.set(links, found)
          setCard({ links, card: found })
        })
        .catch(() => {
          // Refused, rate limited or abandoned: show nothing, and ask again
          // the next time the links change.
        })
    }, DEBOUNCE_MS)
    return () => {
      window.clearTimeout(timer)
      controller.abort()
    }
    // The text matters only through its links; the request sends the text
    // as it stands when the pause ends.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [links, token])

  if (!links) return null
  if (answers.has(links)) return answers.get(links) ?? null
  return card.links === links ? card.card : null
}
