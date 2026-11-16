import type { ReactNode } from 'react'

import { cn } from '@/lib/utils.ts'
import { t } from '@/i18n.tsx'

/**
 * The heading at the top of a column.
 *
 * Mastodon 5.0 standardised these — the announcement counts "at least 12
 * different headers" before it — and gives each column one bar with the same
 * shape: a title that scrolls the column back to the top when clicked, and
 * room on the right for whatever that page can do.
 *
 * The title is not always the navigation's word for the page. Home is labelled
 * "Following", because that is what the feed *is*; the rail already says where
 * you are.
 *
 * Given a `gripRef`, the whole bar is also the handle its column is dragged
 * by — the bar rather than a separate grip icon, because it is already the
 * part of a column that is not the column's contents, and a handle small
 * enough to need aiming at is a handle people miss. What makes it one is the
 * ref: the sortable listens on whatever element it is given, and marks it up
 * for the keyboard and for screen readers.
 */
export function ColumnHeader({
  title,
  children,
  className,
  gripRef,
}: {
  title: string
  /** Controls for this column, aligned right. */
  children?: ReactNode
  className?: string
  /** Present where the column can be moved; absent where it cannot. */
  gripRef?: (element: Element | null) => void
}) {
  return (
    <header
      ref={gripRef}
      // A handle is announced as something to pick up, so it needs a name of
      // its own. Left to the contents it would be read as "Following Close
      // Following" — every control in the bar, run together.
      aria-label={gripRef ? t({ id: 'eunha.column_header.column', defaultMessage: '{title} column' }, { title }) : undefined}
      className={cn(
        // `md:rounded-t-lg` because the corners are the card's, and below `md`
        // there is no card — a radius there would round this bar away from
        // edges it is meant to reach.
        'bg-card/85 sticky top-0 z-30 flex items-center gap-2 border-b px-3 py-2 backdrop-blur md:rounded-t-lg',
        // Always a definite cursor, because the title inherits it: left to
        // `auto` the title's own text would put an I-beam in the middle of a
        // bar you cannot select anything in.
        gripRef ? 'cursor-grab active:cursor-grabbing' : 'cursor-default',
        className,
      )}
    >
      <button
        type="button"
        // `cursor-[inherit]` so the title reads as part of the handle it sits
        // in. The bar sets the cursor, but preflight sets one on every button,
        // and the title is the widest part of the bar — so the grip looked
        // like it stopped at the padding.
        className="min-w-0 flex-1 cursor-[inherit] truncate text-left text-sm font-semibold"
        title={gripRef ? t({ id: 'eunha.column_header.drag_to_reorder_or_press_space', defaultMessage: 'Drag to reorder, or press Space and use the arrows' }) : undefined}
        onClick={() => window.scrollTo({ top: 0, behavior: 'smooth' })}
      >
        {title}
      </button>
      {children && (
        // Not a place to pick the column up from: these are the things a
        // column can do, and closing one is not a way to start moving it.
        //
        // Stopped in the capture phase, which is the only phase that works:
        // the sortable puts a listener of its own on the bar, and React's are
        // delegated to the root, so by the time a bubbling handler here could
        // say no the drag has already begun. The click is a separate event and
        // still reaches the button.
        <div
          onPointerDownCapture={(e) => e.stopPropagation()}
          className="flex shrink-0 items-center gap-1"
        >
          {children}
        </div>
      )}
    </header>
  )
}
