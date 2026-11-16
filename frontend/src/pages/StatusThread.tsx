import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Link, useLocation, useNavigationType, useParams } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getStatus, getStatusContext } from '../api.ts'
import { getActiveAccountId, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { Button } from '@/components/ui/button.tsx'
import { ArrowLeft, LockOpen, Pencil } from 'lucide-react'
import { FeedSkeleton } from '@/components/skeletons.tsx'
import { cn } from '@/lib/utils.ts'
import { t } from '@/i18n.tsx'

// Keeps the window scrolled to the bottom while a thread's images and cards
// load and grow the page, and lets go the moment the reader scrolls, taps or
// types — the same yield `NavigationScroll` gives when it restores a position.
function pinToBottom(): () => void {
  const bottom = () =>
    window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'instant' })
  const observer = new ResizeObserver(bottom)
  const events = ['wheel', 'touchstart', 'pointerdown', 'keydown'] as const
  const stop = () => {
    observer.disconnect()
    window.clearTimeout(timer)
    for (const event of events) window.removeEventListener(event, stop)
  }
  bottom()
  observer.observe(document.body)
  const timer = window.setTimeout(stop, 10_000)
  for (const event of events) window.addEventListener(event, stop, { passive: true })
  return stop
}

// Everyone else in the thread, as the Messages list names them: by display
// name where they have posted in it, by username where they are only
// mentioned. The reader is left out, unless they are talking to themselves.
function participantNames(messages: mastodon.v1.Status[], me: string | null): string {
  const seen = new Map<string, string>()
  for (const m of messages) seen.set(m.account.id, m.account.displayName || m.account.username)
  for (const m of messages) {
    for (const x of m.mentions) if (!seen.has(x.id)) seen.set(x.id, x.username)
  }
  if (me && seen.size > 1) seen.delete(me)
  return [...seen.values()].join(', ')
}

export default function StatusThread() {
  const { id = '' } = useParams()
  const token = getToken()
  const navigationType = useNavigationType()
  const location = useLocation()
  const { openCompose } = useComposeModal()

  const [status, setStatus] = useState<mastodon.v1.Status | null>(null)
  const [context, setContext] = useState<mastodon.v1.Context | null>(null)
  const [contextDone, setContextDone] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // Bumped whenever the view should land on the newest message: on first load
  // and after the reader sends one.
  // Going back or forward leaves the position to `NavigationScroll`, which
  // restores where the reader was; a first load (also a POP, with the
  // location's `default` key) has nothing to restore.
  const [pin, setPin] = useState(navigationType === 'POP' && location.key !== 'default' ? 0 : 1)
  const pinned = useRef(0)

  const load = useCallback(() => {
    getStatus(id, token ?? undefined).then(setStatus).catch((e) => setError(String(e)))
    getStatusContext(id, token ?? undefined)
      .then(setContext)
      .catch(() => {})
      .finally(() => setContextDone(true))
  }, [id, token])

  useEffect(() => {
    setStatus(null)
    setContext(null)
    setContextDone(false)
    setError(null)
    load()
  }, [load])

  // A direct message's thread reads as a chat: every message in the order it
  // was sent, oldest at the top, newest at the bottom, rather than the
  // focused post with its ancestors above and its reply tree below.
  const direct = status?.visibility === 'direct'
  const messages = useMemo(() => {
    if (!direct || !status) return []
    const all = [...(context?.ancestors ?? []), status, ...(context?.descendants ?? [])]
    return all.sort((a, b) => a.createdAt.localeCompare(b.createdAt))
  }, [direct, status, context])
  const latest = messages.at(-1)

  useEffect(() => {
    if (!direct || !contextDone || pin === pinned.current) return
    pinned.current = pin
    // The observer, not a dependency on the messages, follows the thread as it
    // grows: rerunning this on every refetch would cut the pin short.
    return pinToBottom()
  }, [direct, contextDone, pin])

  const reloadAndPin = () => {
    load()
    setPin((n) => n + 1)
  }

  const render = (s: mastodon.v1.Status, detailed = false) => (
    <StatusCard
      key={s.id}
      status={s.reblog ?? s}
      token={token ?? ''}
      filterContext="thread"
      boostedBy={s.reblog ? s.account : undefined}
      detailed={detailed}
      onReply={(replyTo) =>
        openCompose({
          replyTo,
          onPosted: direct ? reloadAndPin : () => load(),
        })
      }
    />
  )

  if (direct && status) {
    const names = participantNames(messages, getActiveAccountId())
    return (
      <div className="page-frame flex flex-col">
        <TopBar />
        <div className="mb-2 flex items-center gap-2">
          <Button
            variant="ghost"
            size="icon"
            render={<Link to="/messages" />}
            aria-label={t({ id: 'tabs_bar.messages', defaultMessage: 'Messages' })}
          >
            <ArrowLeft />
          </Button>
          <h1 className="min-w-0 truncate text-sm font-semibold">{names}</h1>
        </div>

        {!contextDone ? (
          <FeedSkeleton count={3} />
        ) : (
          <>
            <p className="text-muted-foreground mb-2 flex items-start gap-1.5 text-xs">
              <LockOpen className="mt-0.5 size-3.5 shrink-0" />
              {t({ id: 'eunha.messages.messages_are_not_end_to_end', defaultMessage: 'Messages are not end-to-end encrypted. Don\'\'t share anything here you wouldn\'\'t share with the servers involved.' })}
            </p>
            <TimelineStack>
              {messages.map((s) => (
                <div
                  key={s.id}
                  // The post the link pointed at keeps its highlight only when
                  // it is not simply the newest — opening a conversation from
                  // Messages lands on the latest, and marking that is noise.
                  className={cn(s.id === status.id && s.id !== latest?.id && 'bg-primary/5')}
                >
                  {render(s)}
                </div>
              ))}
            </TimelineStack>
          </>
        )}

        {token && latest && (
          <div className="bg-background/85 sticky bottom-0 z-20 mt-auto border-t py-3 backdrop-blur">
            <Button
              className="w-full"
              variant="outline"
              onClick={() => openCompose({ replyTo: latest, onPosted: reloadAndPin })}
            >
              <Pencil /> {t({ id: 'status.reply', defaultMessage: 'Reply' })}
            </Button>
          </div>
        )}
      </div>
    )
  }

  return (
    <div className="page-frame">
      <TopBar />
      {error && <p className="text-destructive text-sm">{error}</p>}

      <div className="space-y-2">
        {!!context?.ancestors.length && (
          <TimelineStack>{context.ancestors.map((s) => render(s))}</TimelineStack>
        )}

        {!status && !error && <FeedSkeleton count={2} />}
        {status && (
          <div className="ring-primary/40 overflow-hidden rounded-md border ring-2">
            {render(status, true)}
          </div>
        )}

        {token && status && (
          <Button
            className="w-full"
            variant="outline"
            onClick={() => openCompose({ replyTo: status, onPosted: () => load() })}
          >
            <Pencil /> {t({ id: 'status.reply', defaultMessage: 'Reply' })}
          </Button>
        )}

        {!!context?.descendants.length && (
          <TimelineStack>{context.descendants.map((s) => render(s))}</TimelineStack>
        )}
      </div>
    </div>
  )
}
