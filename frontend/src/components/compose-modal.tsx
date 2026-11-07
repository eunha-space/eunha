import {
  createContext,
  useCallback,
  useContext,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import { toast } from 'sonner'
import { createPortal } from 'react-dom'
import { Maximize2, Minus, X } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { getActiveAccountId, getToken } from '../auth.ts'
import { Compose } from '@/components/compose.tsx'
import { Button } from '@/components/ui/button.tsx'
import { cn } from '@/lib/utils.ts'

type ComposeOptions = {
  replyTo?: mastodon.v1.Status | null
  quoteOf?: mastodon.v1.Status | null
  // Open as a message. Pass an account to address it, or `null` to open an
  // unaddressed one — the distinction Mastodon 5.0 draws between messaging a
  // person from their profile and starting one from the Messages page.
  messageTo?: mastodon.v1.Account | null
  onPosted?: (status: mastodon.v1.Status) => void
}

type ComposeModalContextValue = {
  openCompose: (options?: ComposeOptions) => void
}

const ComposeModalContext = createContext<ComposeModalContextValue | null>(null)

export function ComposeModalProvider({ children }: { children: ReactNode }) {
  const headingId = useId()
  const [draftVersion, setDraftVersion] = useState(0)
  const [open, setOpen] = useState(false)
  const [dirty, setDirty] = useState(false)
  const closeTimer = useRef<number | undefined>(undefined)
  const [replyTo, setReplyTo] = useState<mastodon.v1.Status | null>(null)
  const [quoteOf, setQuoteOf] = useState<mastodon.v1.Status | null>(null)
  const [messageTo, setMessageTo] = useState<mastodon.v1.Account | null | undefined>(
    undefined,
  )
  // The composer decides its own mode now — picking Direct in the audience menu
  // turns a post into a message without the modal being told twice.
  const [type, setType] = useState<'post' | 'reply' | 'message'>('post')
  // 5.0's composer can be set aside without being thrown away: minimised it is
  // a bar along the bottom, and the page behind it becomes usable again.
  const [minimized, setMinimized] = useState(false)
  // Closing is a state rather than an unmount so the panel can animate out.
  // Without it the composer would vanish on the frame the button is pressed,
  // which is the one moment the motion is actually worth having.
  const [closing, setClosing] = useState(false)
  const [onPosted, setOnPosted] =
    useState<((status: mastodon.v1.Status) => void) | null>(null)

  const finishClose = useCallback(() => {
    setOpen(false)
    setDirty(false)
    setClosing(false)
    setReplyTo(null)
    setQuoteOf(null)
    setMessageTo(undefined)
    setType('post')
    setMinimized(false)
    setOnPosted(null)
  }, [])

  const close = useCallback(() => {
    if (matchMedia('(prefers-reduced-motion: reduce)').matches) {
      finishClose()
      return
    }
    setClosing(true)
    window.clearTimeout(closeTimer.current)
    closeTimer.current = window.setTimeout(finishClose, 140)
  }, [finishClose])

  const openCompose = useCallback((options?: ComposeOptions) => {
    window.clearTimeout(closeTimer.current)
    setClosing(false)
    setMinimized(false)
    const nextMessage = 'messageTo' in (options ?? {}) ? (options?.messageTo ?? null) : undefined
    if (open && dirty && !closing) {
      if (replyTo?.id !== options?.replyTo?.id || quoteOf?.id !== options?.quoteOf?.id
          || messageTo?.id !== nextMessage?.id || (messageTo === undefined) !== (nextMessage === undefined)) {
        toast.info('Finish or close your current draft to start another.')
      }
      return
    }
    setDraftVersion((version) => version + 1)
    setReplyTo(options?.replyTo ?? null)
    setQuoteOf(options?.quoteOf ?? null)
    setMessageTo(nextMessage)
    setOnPosted(() => options?.onPosted ?? null)
    setOpen(true)
  }, [open, dirty, closing, replyTo, quoteOf, messageTo])

  const value = useMemo(() => ({ openCompose }), [openCompose])
  const token = getToken()
  // A reply is a reply even when it is private, matching upstream's
  // `selectComposeType` — reply wins over message.
  const isMessage = type === 'message'

  return (
    <ComposeModalContext.Provider value={value}>
      {children}
      {open && token
        ? createPortal(
            <section
              aria-labelledby={headingId}
              className={cn(
                'text-card-foreground fixed z-50 border shadow-xl',
                // Fit the composer instead of reserving empty height below
                // it. The viewport cap lets the body scroll when replies,
                // attachments, or a long draft need more room.
                minimized
                  ? 'left-1/2 bottom-3 -translate-x-1/2 w-[min(20rem,calc(100vw-1.5rem))] rounded-xl'
                  : 'left-1/2 bottom-3 -translate-x-1/2 max-h-[calc(100dvh-1.5rem)] w-[min(36rem,calc(100vw-1.5rem))] rounded-xl',
                // Motion Eunha adds: upstream swaps the panel in with none.
                closing
                  ? 'motion-safe:animate-out motion-safe:fade-out motion-safe:zoom-out-95 motion-safe:slide-out-to-bottom-2 motion-safe:duration-150'
                  : 'motion-safe:animate-in motion-safe:fade-in motion-safe:zoom-in-95 motion-safe:slide-in-from-bottom-4 motion-safe:duration-200',
                // The ground changes in message mode, which is the whole
                // point of it being a mode: you can see what you are writing
                // into without reading a label.
                isMessage ? 'bg-secondary' : 'bg-card',
                !minimized && 'flex flex-col',
              )}
            >
              <div
                className={cn(
                  'flex shrink-0 items-center justify-between px-4 py-2',
                  !minimized && 'border-b',
                )}
              >
                <h2 id={headingId} className="text-sm font-semibold">
                  {type === 'reply'
                    ? 'Reply'
                    : isMessage
                      ? 'New message'
                      : quoteOf
                        ? 'Quote post'
                        : 'New post'}
                </h2>
                <div className="flex items-center">
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={minimized ? 'Expand composer' : 'Minimise composer'}
                    onClick={() => setMinimized((m) => !m)}
                  >
                    {minimized ? <Maximize2 /> : <Minus />}
                  </Button>
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label="Close"
                    onClick={close}
                  >
                    <X />
                  </Button>
                </div>
              </div>
              {/* Kept mounted while minimised so a draft survives being set
                  aside — that is the whole point of minimising. */}
              <div
                className={cn(
                  'flex flex-1 flex-col overflow-y-auto',
                  minimized && 'hidden',
                )}
              >
                <Compose
                  key={`${getActiveAccountId()}:${draftVersion}`}
                  token={token}
                  replyTo={replyTo}
                  quoteOf={quoteOf}
                  messageTo={messageTo}
                  onTypeChange={setType}
                  onDraftChange={setDirty}
                  onCancelReply={replyTo || quoteOf ? close : undefined}
                  onPosted={(status) => {
                    onPosted?.(status)
                    close()
                  }}
                  framed={false}
                />
              </div>
            </section>,
            document.body,
          )
        : null}
    </ComposeModalContext.Provider>
  )
}

export function useComposeModal() {
  const context = useContext(ComposeModalContext)
  if (!context) throw new Error('useComposeModal must be used inside ComposeModalProvider')
  return context
}
