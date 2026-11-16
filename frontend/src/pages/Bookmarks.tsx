import type { mastodon } from '../masto.ts'
import { getBookmarks } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { FeedSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

export default function Bookmarks() {
  const token = getToken()

  // Bookmarks are ordered by when they were saved, not by post age, and the
  // server's `Link` cursors are bookmark ids rather than status ids — so this
  // walks the paginator instead of paging by the last status's id.
  const feed = useInfinitePaginator<mastodon.v1.Status>(
    () => getBookmarks(token ?? ''),
    [token],
  )
  const statuses = feed.items

  if (!token) {
    return (
      <div className="page-frame">
        <TopBar />
        <Card>
          <CardContent className="space-y-3 py-6 text-center">
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.bookmarks.sign_in_to_see_the_posts', defaultMessage: 'Sign in to see the posts you have bookmarked.' })}
            </p>
            <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-2 text-lg font-bold">{t({ id: 'column.bookmarks', defaultMessage: 'Bookmarks' })}</h1>
      {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
      <div className="space-y-2">
        {statuses === null && !feed.error && (
          <FeedSkeleton />
        )}
        {!!statuses?.length && (
          <TimelineStack>
            {statuses.map((s) => (
              <StatusCard
                key={s.id}
                status={s.reblog ?? s}
                token={token}
                boostedBy={s.reblog ? s.account : undefined}
              />
            ))}
          </TimelineStack>
        )}
        {statuses?.length === 0 && (
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.bookmarks.nothing_bookmarked_yet_the_bookmark_button', defaultMessage: 'Nothing bookmarked yet. The bookmark button on a post saves it here.' })}
          </p>
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
