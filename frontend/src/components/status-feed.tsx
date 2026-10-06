import { useCallback } from 'react'

import type { mastodon } from '../masto.ts'
import { getHomeTimeline, getPublicTimeline } from '../api.ts'
import { useInfiniteFeed } from '../hooks/use-infinite-feed.ts'
import { useStatusStreaming } from '../hooks/use-status-streaming.ts'
import { withoutMessages } from '../lib/statuses.ts'
import { StatusCard } from '@/components/status-card.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'

export type TimelineKind = 'home' | 'local' | 'public'

const EMPTY: Record<TimelineKind, string> = {
  home: 'Your home timeline is empty.',
  local: 'Nothing here yet.',
  public: 'Nothing here yet.',
}

/**
 * One timeline's worth of statuses, with its own pagination and stream.
 *
 * Extracted so a page and a pane render the same feed rather than two copies
 * that drift — the advanced layout shows three of these at once, and a fix to
 * one of them should be a fix to all.
 */
export function StatusFeed({
  kind,
  token,
  onReply,
}: {
  kind: TimelineKind
  token: string | null
  onReply?: (status: mastodon.v1.Status, prepend: (s: mastodon.v1.Status) => void) => void
}) {
  const feed = useInfiniteFeed<mastodon.v1.Status>(
    (maxId) =>
      kind === 'home'
        ? token
          ? getHomeTimeline(token, maxId)
          : Promise.resolve([])
        : getPublicTimeline(kind === 'local', token ?? undefined, maxId),
    [kind, token],
    `timeline:${kind}`,
  )

  const subscribe = useCallback(
    (client: mastodon.streaming.Client) =>
      kind === 'home'
        ? client.user.subscribe()
        : kind === 'local'
          ? client.public.local.subscribe()
          : client.public.subscribe(),
    [kind],
  )

  useStatusStreaming({
    enabled: kind !== 'home' || !!token,
    token: token ?? undefined,
    subscribe,
    feed,
  })

  // Messages are kept out of the home timeline; the public ones never carry
  // them, so the filter is harmless there and applied uniformly.
  const statuses = withoutMessages(feed.items)

  return (
    <>
      {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
      {statuses === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      {!!statuses?.length && (
        <TimelineStack>
          {statuses.map((s) => (
            <StatusCard
              key={s.id}
              status={s.reblog ?? s}
              token={token ?? ''}
              filterContext={kind === 'home' ? 'home' : 'public'}
              boostedBy={s.reblog ? s.account : undefined}
              onReply={onReply ? (status) => onReply(status, feed.prepend) : undefined}
            />
          ))}
        </TimelineStack>
      )}
      {statuses?.length === 0 && (
        <p className="text-muted-foreground text-sm">{EMPTY[kind]}</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!statuses?.length}
      />
    </>
  )
}
