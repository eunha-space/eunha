import { useRef, useState } from 'react'

import type { mastodon } from '../masto.ts'
import { mediaShownByDefault, useReadingPreferences } from '../reading-preferences.ts'
import { Blurhash } from '@/components/blurhash.tsx'
import { Button } from '@/components/ui/button.tsx'
import { ImageViewer } from '@/components/image-viewer.tsx'
import { cn } from '@/lib/utils.ts'

function ImageItem({ m, onOpen, disabled }: {
  m: mastodon.v1.MediaAttachment
  onOpen: (trigger: HTMLButtonElement) => void
  disabled: boolean
}) {
  const [loaded, setLoaded] = useState(false)
  const preview = m.previewUrl || m.url || ''
  const full = m.url || m.remoteUrl || preview

  return (
    <Button variant="ghost" disabled={disabled || !full}
      aria-label={m.description ? `View image: ${m.description}` : 'View image attachment'}
      onClick={event => onOpen(event.currentTarget)}
      className="relative block h-full w-full overflow-hidden rounded-none p-0">
      {m.blurhash && !loaded && (
        <Blurhash hash={m.blurhash} className="absolute inset-0 h-full w-full" />
      )}
      <img
        src={preview || full}
        alt={m.description ?? ''}
        title={m.description ?? undefined}
        loading="lazy"
        onLoad={() => setLoaded(true)}
        className={cn(
          'relative h-full w-full object-cover transition-opacity',
          !loaded && 'opacity-0',
        )}
      />
    </Button>
  )
}

// Mastodon's `MediaGalleryItem`: a GIF plays by itself only when GIFs
// auto-play, and otherwise while the pointer is over it, starting again from
// the top each time.
function GifvItem({ m, autoPlay }: { m: mastodon.v1.MediaAttachment; autoPlay: boolean }) {
  return (
    <video
      src={m.url ?? undefined}
      poster={m.previewUrl}
      aria-label={m.description ?? undefined}
      title={m.description ?? undefined}
      autoPlay={autoPlay}
      loop
      muted
      playsInline
      onMouseEnter={autoPlay ? undefined : (event) => void event.currentTarget.play().catch(() => {})}
      onMouseLeave={autoPlay ? undefined : (event) => {
        event.currentTarget.pause()
        event.currentTarget.currentTime = 0
      }}
      className="h-full w-full object-cover"
    />
  )
}

function MediaItem({ m, onOpen, disabled }: {
  m: mastodon.v1.MediaAttachment
  onOpen: (trigger: HTMLButtonElement) => void
  disabled: boolean
}) {
  const { autoPlayGif } = useReadingPreferences()
  const preview = m.previewUrl || m.url || ''
  const full = m.url || m.remoteUrl || preview

  switch (m.type) {
    case 'image':
      return <ImageItem m={m} onOpen={onOpen} disabled={disabled} />
    case 'gifv':
      // Hidden media does not play behind its cover, whatever the setting.
      // Keyed so that revealing it mounts a player that starts by itself.
      return (
        <GifvItem key={String(autoPlayGif && !disabled)} m={m}
          autoPlay={autoPlayGif && !disabled} />
      )
    case 'video':
      return (
        <video
          src={m.url ?? undefined}
          poster={m.previewUrl}
          controls
          preload="none"
          className="h-full w-full object-cover"
        />
      )
    case 'audio':
      return <audio src={m.url ?? undefined} controls className="w-full" />
    default:
      return (
        <a
          href={full ?? undefined}
          target="_blank"
          rel="noreferrer"
          className="text-accent block p-3 text-sm underline"
        >
          {m.description || 'Attachment'}
        </a>
      )
  }
}

export function MediaAttachments({
  attachments,
  sensitive = false,
  filteredBy = [],
}: {
  attachments: mastodon.v1.MediaAttachment[]
  sensitive?: boolean
  // The titles of the custom filters that blur this post's media here. They
  // hide it whatever the media display setting says.
  filteredBy?: string[]
}) {
  // Whether the media starts shown follows the account's media display
  // setting, as Mastodon's `defaultMediaVisibility` does; the cover's button
  // and the Hide button then toggle it for this post.
  const shownByDefault = mediaShownByDefault(
    useReadingPreferences(),
    sensitive,
    filteredBy.length > 0,
  )
  const [revealed, setRevealed] = useState(shownByDefault)
  // A change to the setting, or to the post, starts the media over from it,
  // as Mastodon's `useRevealedMedia` does when its inputs change.
  const [shownBefore, setShownBefore] = useState(shownByDefault)
  if (shownBefore !== shownByDefault) {
    setShownBefore(shownByDefault)
    setRevealed(shownByDefault)
  }
  const [selected, setSelected] = useState<number | null>(null)
  const returnFocus = useRef<HTMLElement | null>(null)
  const images = attachments.filter(m => m.type === 'image').map(m => ({
    id: m.id, url: m.url || m.remoteUrl || m.previewUrl || '', description: m.description,
  })).filter(image => !!image.url)
  if (attachments.length === 0) return null

  const count = attachments.length
  const allVisual = attachments.every(
    (m) => m.type !== 'audio' && m.type !== 'unknown',
  )

  return (
    <div className="relative mt-2">
      <div
        className={cn(
          'gap-1 overflow-hidden rounded-xl',
          allVisual
            ? cn(
                'grid',
                count === 1 ? 'aspect-[16/10]' : 'aspect-[16/9]',
                count === 1 ? 'grid-cols-1' : 'grid-cols-2',
                count >= 3 && 'grid-rows-2',
              )
            : 'flex flex-col',
          !revealed && 'pointer-events-none',
        )}
      >
        {attachments.map((m, i) => {
          const visual = m.type !== 'audio' && m.type !== 'unknown'
          return (
            <div
              key={m.id}
              className={cn(
                'bg-muted overflow-hidden',
                allVisual && visual && 'h-full w-full',
                // 3-up layout: first image spans the full-height left column
                count === 3 && i === 0 && 'row-span-2',
                !revealed && 'relative',
              )}
            >
              {/* Mastodon draws hidden media as its blurhash and loads none
                  of it until it is shown. */}
              {revealed ? (
                <MediaItem m={m} disabled={false} onOpen={trigger => {
                  const index = images.findIndex(image => image.id === m.id)
                  if (index < 0) return
                  returnFocus.current = trigger
                  setSelected(index)
                }} />
              ) : m.blurhash && visual ? (
                <Blurhash hash={m.blurhash} className="absolute inset-0 h-full w-full" />
              ) : (
                <div className={cn(!allVisual && 'h-12')} />
              )}
            </div>
          )
        })}
      </div>
      <ImageViewer images={images} selected={revealed ? selected : null} onSelect={setSelected}
        onClose={() => setSelected(null)} returnFocus={returnFocus} />
      {revealed ? (
        <Button
          variant="secondary"
          size="xs"
          onClick={() => setRevealed(false)}
          className="bg-background/85 absolute top-2 left-2"
        >
          Hide
        </Button>
      ) : (
        // Mastodon's `SpoilerButton`: what hid the media, then how to show it.
        <button
          type="button"
          onClick={() => setRevealed(true)}
          className="absolute inset-0 flex items-center justify-center"
        >
          <span className="bg-background/85 flex flex-col items-center rounded-md px-3 py-1.5 text-sm">
            <span className="font-medium">
              {filteredBy.length > 0
                ? `Matches filter “${filteredBy.join(', ')}”`
                : sensitive
                  ? 'Sensitive content'
                  : 'Media hidden'}
            </span>
            <span className="text-muted-foreground text-xs">Click to show</span>
          </span>
        </button>
      )}
    </div>
  )
}
