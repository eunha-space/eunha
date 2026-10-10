// The server's announcements, as Mastodon's home column shows them: a button
// in the column's header, badged with how many are unread, that opens them
// above the timeline one at a time, newest first. An announcement is marked
// read once it is the one on screen, and its reactions can be added to. They
// change live as the user stream reports them.
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Megaphone, Plus } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { dismissAnnouncement, getAnnouncements, setAnnouncementReaction } from '../api.ts'
import { AnimateEmoji, EmojiHtml, EmojiText } from '@/components/emoji.tsx'
import { EmojiPicker } from '@/components/emoji-picker.tsx'
import { useStreamingSubscription } from '@/hooks/use-streaming-subscription.ts'
import { Button } from '@/components/ui/button.tsx'
import {
  Carousel,
  CarouselContent,
  CarouselItem,
  CarouselNext,
  CarouselPrevious,
  type CarouselApi,
} from '@/components/ui/carousel.tsx'
import { cn } from '@/lib/utils.ts'

type Announcement = mastodon.v1.Announcement & { read?: boolean }

// What an `announcement.reaction` event carries: the reaction's new count for
// everyone, with no `me`, and which announcement it is on.
type ReactionUpdate = Partial<mastodon.v1.Reaction> & {
  name: string
  count: number
  announcementId?: string | number
}

// Mastodon's `sortAnnouncements`: oldest first by when it starts, or else by
// when it was published. They are shown the other way round.
const sortAnnouncements = (list: Announcement[]) =>
  [...list].sort((a, b) => {
    const at = (x: Announcement) => new Date(x.startsAt ?? x.publishedAt).getTime()
    return at(a) - at(b)
  })

// Mastodon's `updateAnnouncement`: a new one joins the list, a known one takes
// the new fields but keeps what only this reader's own copy knows, whether it
// is read and which reactions are theirs, since the stream sends neither.
function upsertAnnouncement(list: Announcement[], incoming: Announcement) {
  const known = list.find((a) => a.id === incoming.id)
  if (!known) return sortAnnouncements([incoming, ...list])
  const merged: Announcement = {
    ...known,
    ...incoming,
    read: incoming.read ?? known.read,
    reactions: incoming.reactions.map((r) => ({
      ...r,
      me: known.reactions.find((k) => k.name === r.name)?.me ?? !!r.me,
    })),
  }
  return sortAnnouncements(list.map((a) => (a.id === incoming.id ? merged : a)))
}

// Mastodon's `updateReactionCount`: the count from the stream, this reader's
// `me` kept; a reaction not seen before is added as nobody's yet.
function updateReactionCount(list: Announcement[], update: ReactionUpdate) {
  const id = String(update.announcementId)
  return list.map((a) => {
    if (a.id !== id) return a
    const known = a.reactions.some((r) => r.name === update.name)
    return {
      ...a,
      reactions: known
        ? a.reactions.map((r) =>
            r.name === update.name ? { ...r, count: update.count } : r,
          )
        : [
            ...a.reactions,
            {
              name: update.name,
              count: update.count,
              me: false,
              url: update.url ?? '',
              staticUrl: update.staticUrl ?? '',
            },
          ],
    }
  })
}

const subscribeUser = (client: mastodon.streaming.Client) => client.user.subscribe()

export function useAnnouncements(token: string | null) {
  // Kept in Mastodon's order, oldest first.
  const [list, setList] = useState<Announcement[]>([])
  const [shown, setShown] = useState(false)
  useEffect(() => {
    if (!token) return
    let live = true
    getAnnouncements(token)
      .then((fetched) => live && setList(fetched))
      .catch(() => {})
    return () => {
      live = false
    }
  }, [token])

  // Published, edited, reacted to and taken down live, from the user stream
  // Mastodon's home column listens to.
  const onEvent = useCallback((event: mastodon.streaming.Event) => {
    switch (event.event) {
      case 'announcement':
        setList((l) => upsertAnnouncement(l, event.payload))
        break
      case 'announcement.reaction':
        setList((l) => updateReactionCount(l, event.payload as ReactionUpdate))
        break
      case 'announcement.delete': {
        // The id is the bare payload, which masto.js reads as JSON: a number.
        const id = String(event.payload)
        setList((l) => l.filter((a) => a.id !== id))
        break
      }
    }
  }, [])
  useStreamingSubscription({
    enabled: !!token,
    token: token ?? undefined,
    subscribe: subscribeUser,
    onEvent,
  })

  const update = useCallback(
    (id: string, change: (a: Announcement) => Announcement) =>
      setList((l) => l.map((a) => (a.id === id ? change(a) : a))),
    [],
  )
  // Mastodon shows them newest first.
  const items = useMemo(() => [...list].reverse(), [list])
  return {
    items,
    unread: items.filter((a) => !a.read).length,
    shown,
    toggle: () => setShown((s) => !s),
    update,
  }
}

export function AnnouncementsButton({
  unread,
  shown,
  onToggle,
}: {
  unread: number
  shown: boolean
  onToggle: () => void
}) {
  const label = shown ? 'Hide announcements' : 'Show announcements'
  return (
    <Button
      variant="ghost"
      size="icon-sm"
      aria-label={label}
      title={label}
      aria-pressed={shown}
      onClick={onToggle}
      className={cn('relative', shown && 'text-primary')}
    >
      <Megaphone />
      {unread > 0 && (
        <span className="bg-primary text-primary-foreground absolute -top-0.5 -right-0.5 min-w-4 rounded-full px-1 text-[10px] leading-4">
          {unread}
        </span>
      )}
    </Button>
  )
}

const dateFormat = (date: Date, now: Date, withTime: boolean, withDay = true) =>
  new Intl.DateTimeFormat(undefined, {
    year: date.getFullYear() === now.getFullYear() ? undefined : 'numeric',
    month: withDay ? 'short' : undefined,
    day: withDay ? '2-digit' : undefined,
    hour: withTime ? '2-digit' : undefined,
    minute: withTime ? '2-digit' : undefined,
  }).format(date)

// Mastodon's announcement `Timestamp`: the range it runs for when it has one,
// otherwise when it was published; no time of day for an all-day one.
function When({ announcement }: { announcement: Announcement }) {
  const now = new Date()
  const withTime = !announcement.allDay
  if (announcement.startsAt && announcement.endsAt) {
    const starts = new Date(announcement.startsAt)
    const ends = new Date(announcement.endsAt)
    const sameDay = starts.toDateString() === ends.toDateString()
    return (
      <>
        {dateFormat(starts, now, withTime)} - {dateFormat(ends, now, withTime, !sameDay)}
      </>
    )
  }
  return <>{dateFormat(new Date(announcement.publishedAt), now, withTime)}</>
}

function Reactions({
  announcement,
  token,
  onChange,
}: {
  announcement: Announcement
  token: string
  onChange: (reactions: mastodon.v1.Reaction[]) => void
}) {
  const visible = announcement.reactions.filter((r) => r.count > 0)

  const react = (name: string, on: boolean) => {
    const before = announcement.reactions
    const existing = before.find((r) => r.name === name)
    // Shown at once, as Mastodon's reducer does on the request, and put back
    // if the server says no.
    const next = existing
      ? before.map((r) =>
          r.name === name ? { ...r, me: on, count: Math.max(0, r.count + (on ? 1 : -1)) } : r,
        )
      : on
        ? [...before, { name, count: 1, me: true, url: '', staticUrl: '' }]
        : before
    onChange(next)
    setAnnouncementReaction(token, announcement.id, name, on).catch(() => onChange(before))
  }

  return (
    <div className="mt-2 flex flex-wrap items-center gap-1">
      {visible.map((r) => (
        <Button
          key={r.name}
          variant={r.me ? 'secondary' : 'outline'}
          size="xs"
          aria-pressed={r.me}
          onClick={() => react(r.name, !r.me)}
          className={cn('gap-1', r.me && 'border-primary text-primary')}
        >
          {/* A custom reaction carries its own files; a Unicode one is text. */}
          {r.url ? (
            <EmojiText
              text={`:${r.name}:`}
              emojis={[{ shortcode: r.name, url: r.url, staticUrl: r.staticUrl }]}
            />
          ) : (
            <span>{r.name}</span>
          )}
          <span className="tabular-nums">{r.count}</span>
        </Button>
      ))}
      {visible.length < 8 && (
        <EmojiPicker
          label="Add reaction"
          onPick={(shortcode) => react(shortcode, true)}
          trigger={
            <Button variant="ghost" size="icon-xs" aria-label="Add reaction">
              <Plus />
            </Button>
          }
        />
      )}
    </div>
  )
}

function AnnouncementItem({
  announcement,
  token,
  active,
  onChange,
}: {
  announcement: Announcement
  token: string
  active: boolean
  onChange: (change: (a: Announcement) => Announcement) => void
}) {
  // Read once it is the one on screen; drawn as read only once it is not,
  // so the marker does not vanish under the reader's eyes.
  const onChangeRef = useRef(onChange)
  useEffect(() => {
    onChangeRef.current = onChange
  })
  const dismissed = useRef(false)
  useEffect(() => {
    if (active && !announcement.read && !dismissed.current) {
      dismissed.current = true
      dismissAnnouncement(token, announcement.id)
        .then(() => onChangeRef.current((a) => ({ ...a, read: true })))
        .catch(() => {
          dismissed.current = false
        })
    }
  }, [active, announcement.id, announcement.read, token])
  const [visuallyRead, setVisuallyRead] = useState(!!announcement.read)
  const [activeBefore, setActiveBefore] = useState(active)
  if (activeBefore !== active) {
    setActiveBefore(active)
    if (!active && visuallyRead !== !!announcement.read) setVisuallyRead(!!announcement.read)
  }

  return (
    <AnimateEmoji className="relative">
      <p className="text-muted-foreground text-xs font-semibold">
        Announcement · <When announcement={announcement} />
        {!visuallyRead && (
          <span className="bg-primary ml-1.5 inline-block size-2 rounded-full" aria-label="Unread" />
        )}
      </p>
      <EmojiHtml
        className="announcement-content mt-1 text-sm [&_a]:font-medium [&_a]:text-primary [&_a]:underline [&_p+p]:mt-2"
        html={announcement.content}
        emojis={announcement.emojis}
      />
      <Reactions
        announcement={announcement}
        token={token}
        onChange={(reactions) => onChange((a) => ({ ...a, reactions }))}
      />
    </AnimateEmoji>
  )
}

export function AnnouncementsPanel({
  items,
  token,
  update,
}: {
  items: Announcement[]
  token: string
  update: (id: string, change: (a: Announcement) => Announcement) => void
}) {
  const [api, setApi] = useState<CarouselApi>()
  const [index, setIndex] = useState(0)
  useEffect(() => {
    if (!api) return
    const onSelect = () => setIndex(api.selectedScrollSnap())
    onSelect()
    api.on('select', onSelect)
    // One arriving or going live changes the slides under the selection.
    api.on('reInit', onSelect)
    return () => {
      api.off('select', onSelect)
      api.off('reInit', onSelect)
    }
  }, [api])

  if (items.length === 0) return null
  return (
    <section aria-label="Announcements" className="bg-muted/40 border-b px-3 py-3">
      <Carousel setApi={setApi}>
        <CarouselContent>
          {items.map((a, i) => (
            <CarouselItem key={a.id}>
              <AnnouncementItem
                announcement={a}
                token={token}
                active={i === index}
                onChange={(change) => update(a.id, change)}
              />
            </CarouselItem>
          ))}
        </CarouselContent>
        {items.length > 1 && (
          <div className="mt-2 flex items-center justify-end gap-2">
            <span className="text-muted-foreground text-xs tabular-nums">
              {index + 1} / {items.length}
            </span>
            <CarouselPrevious className="static translate-y-0" />
            <CarouselNext className="static translate-y-0" />
          </div>
        )}
      </Carousel>
    </section>
  )
}
