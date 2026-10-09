// A post's link preview, as Mastodon's web client draws `status.card`
// (`features/status/components/card.tsx`): a photo or link card with its
// image, a video card whose player is only loaded on a click, the provider and
// the title, then the author or the description.
//
// The image is hidden behind its blurhash when the post is sensitive or the
// reader hides all media. Unlike media attachments, showing all media does not
// uncover a sensitive post's card: Mastodon's card starts revealed only when
// `!sensitive && displayMedia !== 'hide_all'`. A hidden image is not fetched
// until it is shown, as eunha does for hidden media.
import { useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { ExternalLink, FileText, Play } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { useReadingPreferences } from '../reading-preferences.ts'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { Blurhash } from '@/components/blurhash.tsx'
import { DisplayName } from '@/components/emoji.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { decodeIdna } from '@/lib/punycode.ts'
import { cn } from '@/lib/utils.ts'

// Fields Mastodon's `PreviewCardSerializer` sends that masto.js does not model.
type Card = mastodon.v1.PreviewCard & {
  imageDescription?: string | null
  publishedAt?: string | null
}

// What Mastodon's oEmbed sanitizer gives an embedded player, and so what this
// one gets too: it runs scripts, but on its own origin and in no other window.
const EMBED_SANDBOX =
  'allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms'

/**
 * The player's address, from the card's embed HTML, made to start playing
 * (`handleIframeUrl`): only an `iframe`'s web `src` is taken, never the markup.
 */
function embedSrc(card: Card): string | null {
  if (!card.html) return null
  const doc = new DOMParser().parseFromString(card.html, 'text/html')
  const raw = doc.querySelector('iframe')?.getAttribute('src')
  if (!raw) return null
  try {
    const src = new URL(raw)
    if (src.protocol !== 'https:' && src.protocol !== 'http:') return null
    src.searchParams.set('autoplay', '1')
    src.searchParams.set('auto_play', '1')
    if (card.providerName === 'YouTube') {
      src.searchParams.set('start', new URL(card.url).searchParams.get('t') ?? '')
    }
    return src.href
  } catch {
    return null
  }
}

function hostname(url: string): string {
  try {
    return decodeIdna(new URL(url).hostname)
  } catch {
    return url
  }
}

// Mastodon's `MoreFromAuthor`: the card's author when they have an account.
function MoreFromAuthor({ account }: { account: mastodon.v1.Account }) {
  const avatar = useAnimatedImage(account.avatar, account.avatarStatic)
  return (
    <p className="text-muted-foreground flex items-center gap-1 px-1 pt-1.5 text-xs">
      More from
      <Link
        to={`/@${account.acct}`}
        className="text-foreground inline-flex min-w-0 items-center gap-1 font-medium no-underline hover:underline"
        {...avatar.hover}
      >
        {avatar.src && <img src={avatar.src} alt="" className="size-4 shrink-0 rounded-sm" />}
        <DisplayName account={account} className="truncate" />
      </Link>
    </p>
  )
}

function SpoilerButton({ onReveal }: { onReveal: () => void }) {
  return (
    <button
      type="button"
      onClick={(event) => {
        event.preventDefault()
        event.stopPropagation()
        onReveal()
      }}
      className="absolute inset-0 flex items-center justify-center"
    >
      <span className="bg-background/85 flex flex-col items-center rounded-md px-2 py-1 text-xs">
        <span className="font-medium">Sensitive content</span>
        <span className="text-muted-foreground">Click to show</span>
      </span>
    </button>
  )
}

export function PreviewCard({ card: raw, sensitive }: { card: mastodon.v1.PreviewCard; sensitive: boolean }) {
  const card = raw as Card
  const { displayMedia } = useReadingPreferences()
  const shownByDefault = !sensitive && displayMedia !== 'hide_all'
  const [revealed, setRevealed] = useState(shownByDefault)
  // The setting arriving after the first paint starts the card over from it,
  // as the media beside it does.
  const [shownBefore, setShownBefore] = useState(shownByDefault)
  if (shownBefore !== shownByDefault) {
    setShownBefore(shownByDefault)
    setRevealed(shownByDefault)
  }
  const [loaded, setLoaded] = useState(false)
  const [embedded, setEmbedded] = useState(false)
  const embed = useMemo(() => (card.type === 'video' ? embedSrc(card) : null), [card])

  const interactive = card.type === 'video'
  const image = card.image || null
  const large = (!!image && (card.width ?? 0) > (card.height ?? 0)) || interactive
  const provider = card.providerName || hostname(card.url)
  const author = card.authors?.find((a) => a.account)?.account ?? null
  const description = card.imageDescription ?? ''

  const aspect = interactive ? 'aspect-video' : large ? 'aspect-[1.91/1]' : 'aspect-square'
  const picture = (
    <>
      {card.blurhash && !(revealed && loaded) && (
        <Blurhash hash={card.blurhash} className="absolute inset-0 h-full w-full" />
      )}
      {/* Nothing is fetched behind the cover. */}
      {revealed && image && (
        <img
          src={image}
          alt={description}
          title={description || undefined}
          lang={card.language || undefined}
          loading="lazy"
          onLoad={() => setLoaded(true)}
          className={cn('relative h-full w-full object-cover', !loaded && 'opacity-0')}
        />
      )}
    </>
  )

  let media
  if (interactive && embedded && embed) {
    media = (
      <iframe
        src={embed}
        title={card.title}
        sandbox={EMBED_SANDBOX}
        allow="autoplay; fullscreen; encrypted-media; picture-in-picture"
        allowFullScreen
        referrerPolicy={card.providerName === 'YouTube' ? 'strict-origin-when-cross-origin' : undefined}
        className="aspect-video w-full border-0"
      />
    )
  } else if (interactive) {
    media = (
      <div className={cn('bg-muted relative w-full overflow-hidden', aspect)}>
        {picture}
        {revealed ? (
          <div className="absolute inset-0 flex items-center justify-center gap-2">
            {embed && (
              <button
                type="button"
                aria-label="Play"
                onClick={() => setEmbedded(true)}
                className="bg-background/85 hover:bg-background flex size-10 items-center justify-center rounded-full"
              >
                <Play className="size-5" />
              </button>
            )}
            <a
              href={card.url}
              target="_blank"
              rel="noopener noreferrer"
              aria-label="Open in a new tab"
              className="bg-background/85 hover:bg-background text-foreground flex size-10 items-center justify-center rounded-full"
            >
              <ExternalLink className="size-5" />
            </a>
          </div>
        ) : (
          <SpoilerButton onReveal={() => setRevealed(true)} />
        )}
      </div>
    )
  } else if (image) {
    media = (
      <div
        className={cn(
          'bg-muted relative shrink-0 overflow-hidden',
          large ? 'w-full' : 'w-24 self-stretch sm:w-28',
          aspect,
        )}
      >
        {revealed ? (
          // The whole card is the link, as Mastodon's is; the description
          // beside it carries the accessible name.
          <a href={card.url} target="_blank" rel="noopener noreferrer" tabIndex={-1} aria-hidden className="block h-full w-full">
            {picture}
          </a>
        ) : (
          <>
            {picture}
            <SpoilerButton onReveal={() => setRevealed(true)} />
          </>
        )}
      </div>
    )
  } else {
    media = (
      <a
        href={card.url}
        target="_blank"
        rel="noopener noreferrer"
        tabIndex={-1}
        aria-hidden
        className="bg-muted text-muted-foreground flex w-20 shrink-0 items-center justify-center self-stretch"
      >
        <FileText className="size-6" />
      </a>
    )
  }

  return (
    <div className="mt-2">
      <div
        data-testid="preview-card"
        className={cn('flex overflow-hidden rounded-xl border', large ? 'flex-col' : 'flex-row')}
      >
        {media}
        <a
          href={card.url}
          target="_blank"
          rel="noopener noreferrer"
          dir="auto"
          // A hidden video's description uncovers it rather than leaving.
          onClick={
            interactive && !revealed
              ? (event) => {
                  event.preventDefault()
                  setRevealed(true)
                }
              : undefined
          }
          className="hover:bg-muted/40 flex min-w-0 flex-1 flex-col gap-0.5 p-3 text-sm no-underline"
        >
          <span className="text-muted-foreground truncate text-xs">
            <span lang={card.language || undefined}>{provider}</span>
            {card.publishedAt && (
              <>
                {' · '}
                <RelativeTime value={card.publishedAt} />
              </>
            )}
          </span>
          <strong
            className="text-foreground line-clamp-2 font-semibold"
            title={card.title}
            lang={card.language || undefined}
          >
            {card.title}
          </strong>
          {!author &&
            (card.authorName ? (
              <span className="text-muted-foreground truncate text-xs">
                By <strong className="font-medium">{card.authorName}</strong>
              </span>
            ) : (
              card.description && (
                <span
                  className="text-muted-foreground line-clamp-2 text-xs"
                  lang={card.language || undefined}
                >
                  {card.description}
                </span>
              )
            ))}
        </a>
      </div>
      {author && <MoreFromAuthor account={author} />}
    </div>
  )
}
