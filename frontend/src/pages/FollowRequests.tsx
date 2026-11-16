import { useCallback } from 'react'

import type { mastodon } from '../masto.ts'
import { getFollowRequests } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { AccountRow } from '@/components/account-row.tsx'
import { FollowRequestActions } from '@/components/follow-request-actions.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Button } from '@/components/ui/button.tsx'
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

async function* emptyPages<T>(): AsyncIterable<T[]> {}

export default function FollowRequests() {
  const token = getToken()
  // Follow-request cursors identify request rows, whereas the response contains
  // accounts. Follow the Link header instead of treating an account ID as one.
  const feed = useInfinitePaginator<mastodon.v1.Account>(
    () => (token ? getFollowRequests(token) : emptyPages<mastodon.v1.Account>()),
    [token],
  )
  const { mutate } = feed
  const remove = useCallback(
    (id: string) => mutate((items) => items.filter((a) => a.id !== id)),
    [mutate],
  )
  const accounts = feed.items

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-3 text-lg font-bold">{t({ id: 'column.follow_requests', defaultMessage: 'Follow requests' })}</h1>
      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.follow_requests.sign_in_to_review_your_follow', defaultMessage: 'Sign in to review your follow requests.' })}
          </p>
          <Button size="sm" onClick={() => beginLogin()}>
            {t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}
          </Button>
        </div>
      ) : (
        <div className="space-y-1">
          {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
          {accounts === null && !feed.error && (
            <AccountListSkeleton />
          )}
          {accounts?.map((a) => (
            <AccountRow
              key={a.id}
              account={a}
              action={
                <FollowRequestActions
                  account={a}
                  token={token}
                  onResolved={remove}
                />
              }
            />
          ))}
          {accounts?.length === 0 && (
            <p className="text-muted-foreground text-sm">{t({ id: 'eunha.follow_requests.no_pending_follow_requests', defaultMessage: 'No pending follow requests.' })}</p>
          )}
          <InfiniteScroll
            onLoadMore={feed.loadMore}
            loading={feed.loadingMore}
            done={feed.done}
            hasItems={!!accounts?.length}
          />
        </div>
      )}
    </div>
  )
}
