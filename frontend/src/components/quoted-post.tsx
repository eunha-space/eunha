import { useState, type MouseEvent } from 'react'
import { Link, useNavigate } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { stripQuoteFallback } from '@/lib/content.ts'
import { AnimateEmoji, DisplayName, EmojiHtml, EmojiText } from '@/components/emoji.tsx'
import { MediaAttachments } from '@/components/media-attachments.tsx'
import { Poll } from '@/components/poll.tsx'
import { PreviewCard } from '@/components/preview-card.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { useReadingPreferences } from '../reading-preferences.ts'

// A quote that isn't accepted shows a status message in place of the embedded
// post. Copy mirrors Mastodon's web client (status_quoted.tsx): pending quotes
// stay hidden until the original author's server approves them, so the quoted
// content is never revealed early.
const QUOTE_PLACEHOLDER: Record<string, string> = {
  pending: 'Post pending',
  revoked: 'Post removed by author',
  rejected: 'Post unavailable',
  deleted: 'Post unavailable',
  unauthorized: 'Post unavailable',
  blocked_account: "This post is hidden because you've blocked this account.",
  blocked_domain: "This post is hidden because you've blocked this domain.",
  muted_account: "This post is hidden because you've muted this account.",
}

type Quote = NonNullable<mastodon.v1.Status['quote']>

function quotedStatusOf(quote: Quote): mastodon.v1.Status | null {
  return 'quotedStatus' in quote ? (quote.quotedStatus ?? null) : null
}

function QuotePlaceholder({ quote }: { quote: Quote }) {
  return (
    <div className="text-muted-foreground rounded-md border px-3 py-2 text-xs">
      {QUOTE_PLACEHOLDER[quote.state] ?? 'Post unavailable'}
    </div>
  )
}

// The post embedded by a quote. Like Mastodon, the quoted post is only rendered
// once the quote is accepted; other states (pending approval, revoked, deleted,
// blocked/muted, …) show a message instead.
export function QuotedStatus({ quote, token }: { quote: Quote; token?: string }) {
  const quoted = quotedStatusOf(quote)
  if (quote.state !== 'accepted' || !quoted) return <QuotePlaceholder quote={quote} />
  return <QuotedPost status={quoted} token={token} />
}

// Mastodon's `MAX_QUOTE_POSTS_NESTING_LEVEL` is 1: a quoted post's own quote is
// not drawn again, only named (`NestedQuoteLink`), unless it is unavailable,
// in which case it says so as a quote would.
function NestedQuote({ quote }: { quote: Quote }) {
  const quoted = quotedStatusOf(quote)
  if (quote.state !== 'accepted' || !quoted) return <QuotePlaceholder quote={quote} />
  return (
    <p className="text-muted-foreground mt-1 text-xs">
      Quoted a post by @{quoted.account.acct}
    </p>
  )
}

// Clicks on these inside a quote are theirs, not a click on the quote.
const INTERACTIVE = 'a, button, input, label, video, audio, iframe'

// A compact, read-only rendering of a post embedded inside another: the target
// of a quote, both when displayed on a status card and when previewed in the
// composer. It is what Mastodon's `QuotedStatus` draws through `Status` with
// `isQuotedPost`: the header, the content warning, the content and its poll,
// then the media or, failing media and a quote of its own, the link card — but
// no action bar. `linked` makes a click on it open the thread (off in the
// composer, where navigating away would discard the draft).
export function QuotedPost({
  status,
  linked = true,
  token,
}: {
  status: mastodon.v1.Status
  linked?: boolean
  // Votes in the quoted post's poll; without one the poll shows its results.
  token?: string
}) {
  const navigate = useNavigate()
  const name = status.account.displayName || status.account.username
  const avatar = useAnimatedImage(status.account.avatar, status.account.avatarStatic)
  // A quoted post's content warning folds it as it would a post of its own.
  const { expandSpoilers } = useReadingPreferences()
  const [opened, setOpened] = useState<boolean | null>(null)
  const expanded = opened ?? (expandSpoilers || !status.spoilerText)
  const threadPath = `/@${status.account.acct}/${status.id}`

  // Mastodon's status opens on a click anywhere that is not a control of its
  // own; a link around the whole quote would swallow its media and poll.
  const open = (event: MouseEvent<HTMLDivElement>) => {
    if (!linked || event.defaultPrevented || event.button !== 0) return
    const target = event.target as HTMLElement
    // A portal's events bubble through React to here; the image viewer is
    // not inside the quote on the page.
    if (!event.currentTarget.contains(target) || target.closest(INTERACTIVE)) return
    if (window.getSelection()?.toString()) return
    navigate(threadPath)
  }

  return (
    <div
      data-testid="quoted-post"
      onClick={open}
      className={
        linked
          ? 'hover:bg-accent/40 cursor-pointer rounded-md border p-2'
          : 'rounded-md border p-2'
      }
    >
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
        {linked && (
          <Link
            to={threadPath}
            className="text-muted-foreground ml-auto shrink-0 no-underline hover:underline"
          >
            <RelativeTime value={status.createdAt} />
          </Link>
        )}
      </div>
      {status.spoilerText && (
        <AnimateEmoji className="mt-1 text-sm">
          <span>
            <EmojiText text={status.spoilerText} emojis={status.emojis} />
          </span>
          <button
            type="button"
            onClick={() => setOpened(!expanded)}
            className="text-primary ml-2 text-xs font-medium underline"
          >
            {expanded ? 'Show less' : 'Show more'}
          </button>
        </AnimateEmoji>
      )}
      {expanded && (
        <>
          <EmojiHtml
            className="text-foreground/90 mt-1 line-clamp-6 text-sm [&_a]:underline"
            html={status.quote ? stripQuoteFallback(status.content) : status.content}
            emojis={status.emojis}
          />
          {status.poll && <Poll poll={status.poll} token={token ?? ''} />}
          {status.mediaAttachments.length > 0 && (
            <MediaAttachments
              attachments={status.mediaAttachments}
              sensitive={status.sensitive}
            />
          )}
          {/* As on a post of its own: a card only with no media and no quote. */}
          {status.mediaAttachments.length === 0 && !status.quote && status.card && (
            <PreviewCard
              key={`${status.id}-${status.editedAt}`}
              card={status.card}
              sensitive={status.sensitive}
            />
          )}
          {status.quote && <NestedQuote quote={status.quote} />}
        </>
      )}
    </div>
  )
}
