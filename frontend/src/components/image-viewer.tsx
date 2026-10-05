import { useEffect, useRef, useState, type RefObject } from 'react'
import { ChevronLeft, ChevronRight, ExternalLink, Minimize, X, ZoomIn } from 'lucide-react'

import { Button } from '@/components/ui/button.tsx'
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog.tsx'

export interface ViewerImage {
  id: string
  url: string
  description?: string | null
}

function ImageSlide({ image, original }: { image: ViewerImage; original: boolean }) {
  const [loaded, setLoaded] = useState(false)
  const [failed, setFailed] = useState(false)
  return (
    <>
      {!loaded && !failed && <p role="status" className="absolute text-sm text-white/70">Loading image…</p>}
      {failed ? <p role="alert" className="p-6 text-center text-sm">Could not load this image. You can try opening the original.</p> : (
        <img src={image.url} alt={image.description || 'Image attachment'}
          draggable={false} onLoad={() => setLoaded(true)} onError={() => setFailed(true)}
          className={original ? 'block max-w-none shrink-0' : 'block max-h-full max-w-full object-contain'}
          style={{ opacity: loaded ? 1 : 0 }} />
      )}
    </>
  )
}

/** Accessible, full-screen image gallery shared by timelines and moderation. */
export function ImageViewer({ images, selected, onSelect, onClose, returnFocus }: {
  images: ViewerImage[]
  selected: number | null
  onSelect: (index: number) => void
  onClose: () => void
  returnFocus: RefObject<HTMLElement | null>
}) {
  const [originalFor, setOriginalFor] = useState<string | null>(null)
  const touch = useRef<{ x: number; y: number } | null>(null)
  const stage = useRef<HTMLDivElement>(null)
  const close = useRef<HTMLButtonElement>(null)
  const index = Math.min(selected ?? 0, images.length - 1)
  const image = images[index]
  useEffect(() => {
    if (selected === null) return
    const navigate = (event: KeyboardEvent) => {
      if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
      event.preventDefault()
      event.stopPropagation()
      const next = selected + (event.key === 'ArrowRight' ? 1 : -1)
      if (next < 0 || next >= images.length) return
      setOriginalFor(null)
      stage.current?.scrollTo(0, 0)
      onSelect(next)
    }
    document.addEventListener('keydown', navigate, true)
    return () => document.removeEventListener('keydown', navigate, true)
  }, [selected, images.length, onSelect])
  if (!image) return null
  const original = originalFor === image.id
  const change = (next: number) => {
    if (next < 0 || next >= images.length) return
    setOriginalFor(null)
    if (stage.current) stage.current.scrollTo(0, 0)
    onSelect(next)
  }

  return (
    <Dialog open={selected !== null} onOpenChange={open => {
      if (!open) { setOriginalFor(null); onClose() }
    }}>
      <DialogContent showCloseButton={false} initialFocus={close} finalFocus={returnFocus}
        className="flex h-dvh w-screen max-w-none flex-col gap-0 rounded-none bg-black p-0 text-white ring-0 sm:max-w-none"
        onClick={event => event.stopPropagation()}>
        <header className="flex shrink-0 items-center justify-between gap-2 border-b border-white/15 p-3 pt-[max(0.75rem,env(safe-area-inset-top))]">
          <DialogTitle className="text-sm">Image {index + 1} of {images.length}</DialogTitle>
          <div className="flex items-center gap-1">
            <Button variant="ghost" size="icon" className="hover:bg-white/15 hover:text-white"
              aria-label={original ? 'Fit image to screen' : 'View image at original size'}
              onClick={() => {
                setOriginalFor(original ? null : image.id)
                stage.current?.scrollTo(0, 0)
              }}>{original ? <Minimize /> : <ZoomIn />}</Button>
            <Button variant="ghost" size="icon" className="hover:bg-white/15 hover:text-white"
              aria-label="Open original image in a new tab"
              render={<a href={image.url} target="_blank" rel="noopener noreferrer" />}><ExternalLink /></Button>
            <DialogClose render={<Button ref={close} variant="ghost" size="icon" className="hover:bg-white/15 hover:text-white" aria-label="Close image viewer" />}><X /></DialogClose>
          </div>
        </header>
        <div className="relative min-h-0 flex-1">
          <div ref={stage} className={`absolute inset-0 overflow-auto overscroll-contain ${original ? '' : 'flex items-center justify-center p-2 sm:p-4'}`}
            onTouchStart={event => {
              const point = event.touches[0]
              touch.current = event.touches.length === 1 && point ? { x: point.clientX, y: point.clientY } : null
            }}
            onTouchCancel={() => { touch.current = null }}
            onTouchEnd={event => {
              const point = event.changedTouches[0]
              const start = touch.current
              touch.current = null
              if (original || !point || !start) return
              const dx = point.clientX - start.x
              if (Math.abs(dx) > 60 && Math.abs(point.clientY - start.y) < Math.abs(dx) / 2) change(index + (dx < 0 ? 1 : -1))
            }}>
            <ImageSlide key={image.id} image={image} original={original} />
          </div>
        </div>
        <footer className="shrink-0 border-t border-white/15 p-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]">
          {images.length > 1 && <div className="mb-2 flex items-center justify-center gap-4">
            <Button size="sm" variant="ghost" className="hover:bg-white/15 hover:text-white" aria-label="Previous image" disabled={index === 0} onClick={() => change(index - 1)}><ChevronLeft /> Previous</Button>
            <span aria-live="polite" className="text-xs tabular-nums">{index + 1} / {images.length}</span>
            <Button size="sm" variant="ghost" className="hover:bg-white/15 hover:text-white" aria-label="Next image" disabled={index === images.length - 1} onClick={() => change(index + 1)}>Next <ChevronRight /></Button>
          </div>}
          <DialogDescription className="max-h-[20dvh] overflow-auto whitespace-pre-wrap text-center text-sm text-white/80">
            {image.description || 'No image description provided.'}
          </DialogDescription>
        </footer>
      </DialogContent>
    </Dialog>
  )
}
