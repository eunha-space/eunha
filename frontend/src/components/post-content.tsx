// The one limit on how much of a post's text is drawn before the reader asks
// for the rest, wherever a post appears: timelines, notifications, search,
// profiles, conversations, quoted posts and edit history. Only the focused
// post of a thread is drawn whole, as it is the place to read one.
//
// Two limits, both lifted together by "Read more":
//
//  -  Height, Mastodon's `StatusContent`: text taller than 35 of its own
//     lines is cut to 35, its last 80px faded out.
//  -  Length: past `TEXT_LIMIT` characters only that many are parsed and
//     drawn at all. A height cut alone still has the browser build and lay
//     out every character under it, and a remote post can be up to the
//     megabyte an inbox accepts; a timeline of those would hang the page.
//
// Unlike Mastodon's "View full post", "Read more" opens the post in place
// rather than going to it.
import { useLayoutEffect, useMemo, useRef, useState } from 'react'

import { EmojiHtml, type EmojiList } from '@/components/emoji.tsx'
import { t } from '@/i18n.tsx'
import { truncateHtml } from '@/lib/content.ts'
import { cn } from '@/lib/utils.ts'

/** Text taller than this many of its lines is cut to that many. */
export const MAX_LINES = 35
/**
 * Past this many characters, text is cut whatever its height, and no more of
 * it is drawn until it is opened: far more than 35 lines at any column width.
 */
export const TEXT_LIMIT = 5000

export function PostContent({
  html,
  emojis,
  className,
  whole = false,
  wrapperClassName,
  buttonClassName,
}: {
  /** Server-sanitized HTML, as `EmojiHtml` takes it. */
  html: string
  emojis: EmojiList
  /** The text's own classes, which set the line height counted in. */
  className?: string
  /** Draw it all, as the focused post of a thread is. */
  whole?: boolean
  wrapperClassName?: string
  buttonClassName?: string
}) {
  const ref = useRef<HTMLDivElement>(null)
  const [open, setOpen] = useState(false)
  const full = whole || open
  const short = useMemo(() => (full ? null : truncateHtml(html, TEXT_LIMIT)), [full, html])
  // The height the text is cut to, or `null` while it fits.
  const [limit, setLimit] = useState<number | null>(null)

  useLayoutEffect(() => {
    const node = ref.current
    const content = node?.firstElementChild
    if (!node || !content || whole) {
      setLimit(null)
      return
    }
    const measure = () => {
      const style = getComputedStyle(content)
      const line = parseFloat(style.lineHeight) || parseFloat(style.fontSize) * 1.2 || 20
      // `scrollHeight` is the whole text's height even while the box is cut.
      const tall = short?.truncated || node.scrollHeight > line * MAX_LINES
      setLimit((previous) => (tall ? line * MAX_LINES : open ? previous : null))
    }
    measure()
    // The text's own box keeps growing while the wrapper is cut: a
    // translation, an edit, or emoji and fonts arriving change it.
    const observer = new ResizeObserver(measure)
    observer.observe(content)
    return () => observer.disconnect()
  }, [whole, open, short])

  const cut = limit !== null && !open
  return (
    <div className={wrapperClassName}>
      <div
        ref={ref}
        className={cn(cut && 'post-content-cut overflow-hidden')}
        style={cut ? { maxHeight: limit } : undefined}
      >
        <EmojiHtml className={className} html={short ? short.html : html} emojis={emojis} />
      </div>
      {limit !== null && (
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((o) => !o)}
          className={cn('text-primary mt-1 text-xs font-medium underline', buttonClassName)}
        >
          {open
            ? t({ id: 'block_modal.show_less', defaultMessage: 'Show less' })
            : t({ id: 'eunha.post_content.read_more', defaultMessage: 'Read more' })}
        </button>
      )}
    </div>
  )
}
