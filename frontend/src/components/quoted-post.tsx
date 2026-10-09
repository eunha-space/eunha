import { useState } from 'react'
import { Link } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { stripQuoteFallback } from '@/lib/content.ts'
import { AnimateEmoji, DisplayName, EmojiHtml, EmojiText } from '@/components/emoji.tsx'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { useReadingPreferences } from '../reading-preferences.ts'

// A compact, read-only rendering of a post embedded inside another: the target
// of a quote, both when displayed on a status card and when previewed in the
// composer. `linked` wraps it in a link to the thread (off in the composer,
// where navigating away would discard the draft).
export function QuotedPost({
  status,
  linked = true,
}: {
  status: mastodon.v1.Status
  linked?: boolean
}) {
  const name = status.account.displayName || status.account.username
  const avatar = useAnimatedImage(status.account.avatar, status.account.avatarStatic)
  // A quoted post's content warning folds it as it would a post of its own.
  const { expandSpoilers } = useReadingPreferences()
  const [opened, setOpened] = useState<boolean | null>(null)
  const expanded = opened ?? (expandSpoilers || !status.spoilerText)
  const inner = (
    <>
      <div className="flex min-w-0 items-center gap-1.5 text-xs">
        <Avatar className="size-4" {...avatar.hover}>
          <AvatarImage src={avatar.src} alt="" />
          <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
        </Avatar>
        <DisplayName
          account={status.account}
          className="text-foreground truncate font-semibold"
        />
        <span className="text-muted-foreground truncate">
          @{status.account.acct}
        </span>
      </div>
      {status.spoilerText && (
        <AnimateEmoji className="mt-1 text-sm">
          <span>
            <EmojiText text={status.spoilerText} emojis={status.emojis} />
          </span>
          <button
            type="button"
            onClick={(event) => {
              // The card around it is a link to the post.
              event.preventDefault()
              event.stopPropagation()
              setOpened(!expanded)
            }}
            className="text-primary ml-2 text-xs font-medium underline"
          >
            {expanded ? 'Show less' : 'Show more'}
          </button>
        </AnimateEmoji>
      )}
      {expanded && (
        <EmojiHtml
          className="text-foreground/90 mt-1 line-clamp-6 text-sm [&_a]:underline"
          html={status.quote ? stripQuoteFallback(status.content) : status.content}
          emojis={status.emojis}
        />
      )}
      {expanded && status.mediaAttachments.length > 0 && (
        <p className="text-muted-foreground mt-1 text-xs">
          {status.mediaAttachments.length} attachment
          {status.mediaAttachments.length > 1 ? 's' : ''}
        </p>
      )}
    </>
  )

  if (!linked) return <div className="rounded-md border p-2">{inner}</div>

  return (
    <Link
      to={`/@${status.account.acct}/${status.id}`}
      className="hover:bg-accent/40 block rounded-md border p-2 no-underline"
    >
      {inner}
    </Link>
  )
}
