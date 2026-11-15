import { useEffect, useState, type MouseEvent } from 'react'
import { Link, useNavigate } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getCachedInstance, getRelationship, getStatus } from '../api.ts'
import { getMeId } from '../me.ts'
import { Button } from '@/components/ui/button.tsx'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { stripQuoteFallback } from '@/lib/content.ts'
import { cn } from '@/lib/utils.ts'
import { AnimateEmoji, DisplayName, EmojiHtml, EmojiText } from '@/components/emoji.tsx'
import { MediaAttachments } from '@/components/media-attachments.tsx'
import { Poll } from '@/components/poll.tsx'
import { PreviewCard } from '@/components/preview-card.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { useReadingPreferences } from '../reading-preferences.ts'

// A quote that isn't accepted shows a status message in place of the embedded
// post. Copy mirrors Mastodon's web client (quote.tsx): pending quotes stay
// hidden until the original author's server approves them, so the quoted
// content is never revealed early.
const QUOTE_PLACEHOLDER: Record<string, string> = {
  pending: 'Post pending',
  revoked: 'Post removed by author',
}

// A quote of someone the reader blocked or muted names them, and can be shown
// anyway: the server sends the quoted post with these states.
function hiddenByReader(quote: Quote, account: mastodon.v1.Account): string | null {
  switch (quote.state) {
    case 'blocked_account':
      return `This post is hidden because you've blocked @${account.acct}.`
    case 'blocked_domain':
      return `This post is hidden because you've blocked ${account.acct.split('@')[1]}.`
    case 'muted_account':
      return `This post is hidden because you've muted @${account.acct}.`
    default:
      return null
  }
}

// Mastodon's `getAccountHidden`: an account this server's moderators limited
// is hidden from whoever neither follows it nor has asked to.
function useAccountHidden(account: mastodon.v1.Account | null, token?: string): boolean {
  const limited = !!account?.limited && account.id !== getMeId()
  const id = account?.id
  const [following, setFollowing] = useState<{ id: string; following: boolean }>()
  useEffect(() => {
    if (!limited || !id || !token) return
    let active = true
    getRelationship(id, token)
      .then((r) => { if (active) setFollowing({ id, following: !!(r?.following || r?.requested) }) })
      .catch(() => {})
    return () => { active = false }
  }, [limited, id, token])
  return limited && !(following?.id === id && following?.following)
}

type Quote = NonNullable<mastodon.v1.Status['quote']>

function quotedStatusOf(quote: Quote): mastodon.v1.Status | null {
  return 'quotedStatus' in quote ? (quote.quotedStatus ?? null) : null
}

// The post a quote names. A quote is sent with its post inside it only at the
// top: the quoted post's own quote comes shallow, naming its post by id
// (`quoted_status_id`), as Mastodon's `ShallowQuoteSerializer` sends it, and
// is fetched the way Mastodon's `QuotedStatus` fetches a quote it has not
// got. `undefined` while that is under way, `null` when there is none.
function useQuotedStatus(quote: Quote, token?: string): mastodon.v1.Status | null | undefined {
  const embedded = quotedStatusOf(quote)
  const id = 'quotedStatusId' in quote ? (quote.quotedStatusId ?? null) : null
  const wanted = !embedded && !!id && quote.state !== 'deleted'
  const [fetched, setFetched] = useState<{ id: string; status: mastodon.v1.Status | null }>()
  useEffect(() => {
    if (!wanted || !id) return
    let active = true
    getStatus(id, token || undefined)
      .then((status) => { if (active) setFetched({ id, status }) })
      .catch(() => { if (active) setFetched({ id, status: null }) })
    return () => { active = false }
  }, [wanted, id, token])
  if (embedded) return embedded
  if (!wanted) return null
  return fetched?.id === id ? fetched.status : undefined
}

// Mastodon's "Learn more" beside "Post pending", which says why it waits.
function PendingInfo() {
  return (
    <Popover>
      <PopoverTrigger
        render={<Button variant="link" size="sm" className="h-auto p-0 text-xs" />}
      >
        Learn more
      </PopoverTrigger>
      <PopoverContent align="end" className="w-56 text-xs">
        <p>
          On Mastodon, you can control whether someone can quote you. This post is pending
          while we're getting the original author's approval.
        </p>
      </PopoverContent>
    </Popover>
  )
}

function QuotePlaceholder({
  quote,
  message,
  onShow,
}: {
  quote?: Quote
  message?: string
  // Mastodon's "Show anyway", for a quote hidden by the reader's own choice.
  onShow?: () => void
}) {
  return (
    <div className="text-muted-foreground flex flex-wrap items-center gap-x-2 rounded-md border px-3 py-2 text-xs">
      <span>{message ?? QUOTE_PLACEHOLDER[quote?.state ?? ''] ?? 'Post unavailable'}</span>
      {!message && quote?.state === 'pending' && <PendingInfo />}
      {onShow && (
        <Button variant="link" size="sm" className="h-auto p-0 text-xs" onClick={onShow}>
          Show anyway
        </Button>
      )}
    </div>
  )
}

interface QuoteFilters {
  // Every filter of the context the quoted post matches, blurring ones too,
  // as Mastodon's `selectStatusFilters` lists them.
  titles: string[]
  action: 'hide' | 'warn' | null
  // Those that hide or warn, which Mastodon's `FilterWarning` names.
  warnedBy: string[]
  blurredBy: string[]
}

const NO_FILTERS: QuoteFilters = { titles: [], action: null, warnedBy: [], blurredBy: [] }

// Mastodon's `selectStatusFilters` for a quoted post, in the quoting post's
// filter context: a `hide` filter hides it, any other but `blur` warns, and
// the reader's own posts are never filtered.
function quoteFilters(
  status: mastodon.v1.Status,
  context?: mastodon.v2.FilterContext,
): QuoteFilters {
  if (!context || status.account.id === getMeId()) return NO_FILTERS
  const matches = (status.filtered ?? []).filter((r) => r.filter.context.includes(context))
  let action: QuoteFilters['action'] = null
  for (const { filter } of matches) {
    if (filter.filterAction === 'hide') action = 'hide'
    else if (filter.filterAction !== 'blur') action ??= 'warn'
  }
  return {
    titles: matches.map((r) => r.filter.title),
    action,
    warnedBy: matches.filter((r) => r.filter.filterAction !== 'blur').map((r) => r.filter.title),
    blurredBy: matches.filter((r) => r.filter.filterAction === 'blur').map((r) => r.filter.title),
  }
}

const listFormat = new Intl.ListFormat('en', { type: 'conjunction' })

// The post embedded by a quote. Like Mastodon, the quoted post is only rendered
// once the quote is accepted; other states (pending approval, revoked, deleted,
// blocked/muted, …) show a message instead.
//
// `detailed` is the focused post of a thread, which Mastodon draws with its
// legacy `QuotedStatus`; elsewhere it draws its redesigned `StatusQuote`. The
// two say a filtered quote differently: the first "Hidden due to one of your
// filters", folding a warned one behind a banner; the second names the
// filters, and offers "Show anyway" for a warning.
export function QuotedStatus({
  quote,
  token,
  filterContext,
  detailed = false,
}: {
  quote: Quote
  token?: string
  filterContext?: mastodon.v2.FilterContext
  detailed?: boolean
}) {
  // Mastodon's "Show anyway", for a filter's warning or a block or mute.
  const [revealed, setRevealed] = useState(false)
  const [accountRevealed, setAccountRevealed] = useState(false)
  const quoted = quotedStatusOf(quote)
  const accountHidden = useAccountHidden(quoted?.account ?? null, token)
  const filters = quoted ? quoteFilters(quoted, filterContext) : NO_FILTERS
  const filteredMessage = `Hidden due to your filter “${listFormat.format(filters.titles)}”`
  const hiddenMessage = quoted && hiddenByReader(quote, quoted.account)

  // In the order Mastodon's `useQuoteError` tries them.
  if (quote.state === 'pending') return <QuotePlaceholder quote={quote} />
  if (filters.action === 'hide') {
    return (
      <QuotePlaceholder
        message={detailed ? 'Hidden due to one of your filters' : filteredMessage}
      />
    )
  }
  if (quote.state === 'revoked') return <QuotePlaceholder quote={quote} />
  if (!detailed && filters.action === 'warn' && !revealed) {
    return <QuotePlaceholder message={filteredMessage} onShow={() => setRevealed(true)} />
  }
  if (hiddenMessage && !revealed) {
    return <QuotePlaceholder message={hiddenMessage} onShow={() => setRevealed(true)} />
  }
  if (!quoted || (quote.state !== 'accepted' && !hiddenMessage)) {
    return <QuotePlaceholder message="Post unavailable" />
  }
  if (accountHidden && !accountRevealed) {
    const domain = getCachedInstance()?.domain ?? window.location.hostname
    return (
      <QuotePlaceholder
        message={`This account has been hidden by the moderators of ${domain}.`}
        onShow={() => setAccountRevealed(true)}
      />
    )
  }
  return (
    <QuotedPost
      status={quoted}
      token={token}
      variant={detailed ? 'full' : 'compact'}
      filteredBy={detailed && filters.action === 'warn' ? filters.warnedBy : []}
      mediaFilteredBy={filters.blurredBy}
    />
  )
}

// Mastodon's `MAX_QUOTE_POSTS_NESTING_LEVEL` is 1: a quoted post's own quote is
// not drawn again, only named (`NestedQuoteLink`), unless it is unavailable,
// in which case it says so as a quote would.
function NestedQuote({ quote, token }: { quote: Quote; token?: string }) {
  const quoted = useQuotedStatus(quote, token)
  const [revealed, setRevealed] = useState(false)
  // Nothing to name yet while its post is fetched.
  if (quoted === undefined) return null
  // One by someone the reader blocked or muted says so, as the quote would.
  const hiddenMessage = quoted && hiddenByReader(quote, quoted.account)
  if (hiddenMessage && !revealed) {
    return <QuotePlaceholder message={hiddenMessage} onShow={() => setRevealed(true)} />
  }
  if ((quote.state !== 'accepted' && !hiddenMessage) || !quoted) {
    return <QuotePlaceholder quote={quote} />
  }
  return (
    <p className="text-muted-foreground mt-1 text-xs">
      Quoted a post by @{quoted.account.acct}
    </p>
  )
}

// The compact quote's own quote: Mastodon's `QuotedStatusLink`, "Post from
// {author}", linking to it, and nothing at all while it is not there to name.
function NestedQuoteLink({ quote, token, linked }: { quote: Quote; token?: string; linked: boolean }) {
  const quoted = useQuotedStatus(quote, token)
  if (quote.state !== 'accepted' || !quoted) return null
  const label = (
    <>
      <strong className="font-semibold">
        Post from <DisplayName account={quoted.account} />
      </strong>
      {' • '}
      <RelativeTime value={quoted.createdAt} />
    </>
  )
  return (
    <p className="text-muted-foreground mt-1 truncate text-xs">
      {linked ? (
        <Link to={`/@${quoted.account.acct}/${quoted.id}`} className="text-muted-foreground no-underline hover:underline">
          {label}
        </Link>
      ) : (
        label
      )}
    </p>
  )
}

// The compact quote's link card, which it names by its address rather than
// drawing: an address on this server is followed in the app.
function CardLink({ card, linked }: { card: mastodon.v1.PreviewCard; linked: boolean }) {
  let local = false
  try {
    local = new URL(card.url).host === window.location.host
  } catch {
    // Not an address to follow in the app, then.
  }
  const className = 'text-muted-foreground mt-1 block truncate text-xs underline'
  if (!linked) return <p className={className}>{card.url}</p>
  if (local) {
    const url = new URL(card.url)
    return <Link to={url.pathname + url.search + url.hash} className={className}>{card.url}</Link>
  }
  return (
    <a href={card.url} target="_blank" rel="noopener" className={className}>
      {card.url}
    </a>
  )
}

// Mastodon's `QuotedStatusBody` and the `QuotedStatusLink` under it.
function CompactBody({
  status,
  token,
  linked,
  mediaFilteredBy,
}: {
  status: mastodon.v1.Status
  token?: string
  linked: boolean
  mediaFilteredBy: string[]
}) {
  if (status.spoilerText) {
    return (
      <AnimateEmoji className="text-muted-foreground mt-1 text-sm">
        Content:{'\u00a0'}
        <EmojiText text={status.spoilerText} emojis={status.emojis} />
      </AnimateEmoji>
    )
  }
  const html = status.quote ? stripQuoteFallback(status.content) : status.content
  return (
    <>
      {status.inReplyToAccountId === status.account.id && (
        <p className="text-muted-foreground mt-1 text-xs">Continued thread</p>
      )}
      {html.trim() && (
        <EmojiHtml
          // Clamped in the composer, as Mastodon's preview is; whole elsewhere.
          className={cn('rich-text text-foreground/90 mt-1 text-sm [&_a]:underline', !linked && 'line-clamp-6')}
          html={html}
          emojis={status.emojis}
        />
      )}
      {status.poll && (
        <p className="text-muted-foreground mt-1 text-sm">
          Poll • {status.poll.expired ? 'Closed' : 'Accepting responses'}
        </p>
      )}
      {status.mediaAttachments.length > 0 && (
        <MediaAttachments
          attachments={status.mediaAttachments}
          sensitive={status.sensitive}
          filteredBy={mediaFilteredBy}
        />
      )}
      {status.card ? (
        <CardLink card={status.card} linked={linked} />
      ) : (
        status.quote && <NestedQuoteLink quote={status.quote} token={token} linked={linked} />
      )}
    </>
  )
}

// Clicks on these inside a quote are theirs, not a click on the quote.
const INTERACTIVE = 'a, button, input, label, video, audio, iframe'

// A read-only rendering of a post embedded inside another: the target of a
// quote, both when displayed on a status card and when previewed in the
// composer. It has no action bar, and comes in Mastodon's two forms:
//
//  -  `full`, on a thread's focused post, is what Mastodon's legacy
//     `QuotedStatus` draws through `Status` with `isQuotedPost`: the header,
//     the content warning, the content and its poll, then the media or,
//     failing media and a quote of its own, the link card, and its own quote
//     named.
//  -  `compact`, everywhere else and in the composer, is its redesigned
//     `QuotedStatus`: a content warning stands in for everything under it
//     ("Content: …", not to be opened here), a poll is only said to be one,
//     and the link card and its own quote are each a link, the card's
//     winning, and neither under a content warning.
//
// `linked` makes a click on it open the thread (off in the composer, where
// navigating away would discard the draft).
export function QuotedPost({
  status,
  linked = true,
  variant = 'compact',
  token,
  filteredBy = [],
  mediaFilteredBy = [],
}: {
  status: mastodon.v1.Status
  linked?: boolean
  variant?: 'full' | 'compact'
  // Votes in the quoted post's poll; without one the poll shows its results.
  token?: string
  // Warning filters it matches, which fold it behind Mastodon's
  // `FilterWarning` until shown.
  filteredBy?: string[]
  // Filters that blur its media.
  mediaFilteredBy?: string[]
}) {
  const navigate = useNavigate()
  const name = status.account.displayName || status.account.username
  const avatar = useAnimatedImage(status.account.avatar, status.account.avatarStatic)
  // A quoted post's content warning folds it as it would a post of its own.
  const { expandSpoilers } = useReadingPreferences()
  const [opened, setOpened] = useState<boolean | null>(null)
  const [showDespiteFilter, setShowDespiteFilter] = useState(false)
  const filtered = filteredBy.length > 0 && !showDespiteFilter
  const expanded = !filtered && (opened ?? (expandSpoilers || !status.spoilerText))
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
      {filteredBy.length > 0 && (
        <div className="text-muted-foreground mt-1 flex flex-wrap items-center gap-x-2 text-sm">
          <span>Matches filter “{filteredBy.join(', ')}”</span>
          <button
            type="button"
            onClick={() => setShowDespiteFilter(!showDespiteFilter)}
            className="text-primary text-xs font-medium underline"
          >
            {showDespiteFilter ? 'Hide post' : 'Show anyway'}
          </button>
        </div>
      )}
      {variant === 'compact' && (
        <CompactBody status={status} token={token} linked={linked} mediaFilteredBy={mediaFilteredBy} />
      )}
      {variant === 'full' && !filtered && status.spoilerText && (
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
      {variant === 'full' && expanded && (
        <>
          <EmojiHtml
            className="rich-text text-foreground/90 mt-1 text-sm [&_a]:underline"
            html={status.quote ? stripQuoteFallback(status.content) : status.content}
            emojis={status.emojis}
          />
          {status.poll && <Poll poll={status.poll} token={token ?? ''} />}
          {status.mediaAttachments.length > 0 && (
            <MediaAttachments
              attachments={status.mediaAttachments}
              sensitive={status.sensitive}
              filteredBy={mediaFilteredBy}
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
          {status.quote && <NestedQuote quote={status.quote} token={token} />}
        </>
      )}
    </div>
  )
}
