import { useCallback } from 'react'
import { useParams } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getTagTimeline } from '../api.ts'
import { getToken } from '../auth.ts'
import { useInfiniteFeed } from '../hooks/use-infinite-feed.ts'
import { useStatusStreaming } from '../hooks/use-status-streaming.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'

export default function TagTimeline() {
  const { name = '' } = useParams()
  const token = getToken()

  const feed = useInfiniteFeed<mastodon.v1.Status>(
    (maxId) => getTagTimeline(name, token ?? undefined, maxId),
    [name, token],
  )
  const subscribeTag = useCallback(
    (client: mastodon.streaming.Client) => client.hashtag.subscribe({ tag: name }),
    [name],
  )

  useStatusStreaming({
    enabled: !!name,
    token: token ?? undefined,
    subscribe: subscribeTag,
    feed,
  })
  const statuses = feed.items

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-4 text-xl font-bold">#{name}</h1>
      {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
      <div className="space-y-2">
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
                filterContext="public"
                boostedBy={s.reblog ? s.account : undefined}
              />
            ))}
          </TimelineStack>
        )}
        {statuses?.length === 0 && (
          <p className="text-muted-foreground text-sm">No posts yet.</p>
        )}
        <InfiniteScroll
          onLoadMore={feed.loadMore}
          loading={feed.loadingMore}
          done={feed.done}
          hasItems={!!statuses?.length}
        />
      </div>
    </div>
  )
}
