import { useRef, useState } from 'react'

import type { mastodon } from '../masto.ts'
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

function MediaItem({ m, onOpen, disabled }: {
  m: mastodon.v1.MediaAttachment
  onOpen: (trigger: HTMLButtonElement) => void
  disabled: boolean
}) {
  const preview = m.previewUrl || m.url || ''
  const full = m.url || m.remoteUrl || preview

  switch (m.type) {
    case 'image':
      return <ImageItem m={m} onOpen={onOpen} disabled={disabled} />
    case 'gifv':
      return (
        <video
          src={m.url ?? undefined}
          poster={m.previewUrl}
          autoPlay
          loop
          muted
          playsInline
          className="h-full w-full object-cover"
        />
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
  sensitive,
}: {
  attachments: mastodon.v1.MediaAttachment[]
  sensitive?: boolean
}) {
  const [revealed, setRevealed] = useState(!sensitive)
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
                !revealed && 'blur-xl',
              )}
            >
              <MediaItem m={m} disabled={!revealed} onOpen={trigger => {
                if (!revealed) return
                const index = images.findIndex(image => image.id === m.id)
                if (index < 0) return
                returnFocus.current = trigger
                setSelected(index)
              }} />
            </div>
          )
        })}
      </div>
      <ImageViewer images={images} selected={revealed ? selected : null} onSelect={setSelected}
        onClose={() => setSelected(null)} returnFocus={returnFocus} />
      {!revealed && (
        <button
          type="button"
          onClick={() => setRevealed(true)}
          className="absolute inset-0 flex items-center justify-center"
        >
          <span className="bg-background/85 rounded-md px-3 py-1.5 text-sm font-medium">
            Sensitive content — show
          </span>
        </button>
      )}
    </div>
  )
}
