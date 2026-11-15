import { useEffect, useState } from 'react'
import { NavLink, useLocation, useParams } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getFavouritedBy, getRebloggedBy, getStatus } from '../api.ts'
import { getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { AccountRow } from '@/components/account-row.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { cn } from '@/lib/utils.ts'
import { AccountListSkeleton } from '@/components/skeletons.tsx'

const tab =
  'border-b-2 border-transparent px-3 py-2 text-sm font-medium text-muted-foreground no-underline hover:text-foreground'
const cls = ({ isActive }: { isActive: boolean }) =>
  cn(tab, isActive && 'border-primary text-foreground')

export default function StatusReactions() {
  const { acct = '', id = '' } = useParams()
  const handle = acct.replace(/^@/, '')
  const boosts = useLocation().pathname.endsWith('/reblogs')
  const token = getToken()

  const [status, setStatus] = useState<mastodon.v1.Status | null>(null)
  const [statusError, setStatusError] = useState<string | null>(null)

  useEffect(() => {
    setStatus(null)
    setStatusError(null)
    getStatus(id, token ?? undefined)
      .then(setStatus)
      .catch((e) => setStatusError(String(e)))
  }, [id, token])

  const feed = useInfinitePaginator<mastodon.v1.Account>(
    () =>
      boosts
        ? getRebloggedBy(id, token ?? undefined)
        : getFavouritedBy(id, token ?? undefined),
    [id, boosts, token],
  )
  const accounts = feed.items
  // A post nobody may see 404s on both requests; say so once rather than
  // showing an empty list under a missing post.
  const error = statusError ?? feed.error

  return (
    <div className="page-frame">
      <TopBar />
      {status && (
        <div className="mb-2 overflow-hidden rounded-md border">
          <StatusCard status={status} token={token ?? ''} />
        </div>
      )}
      <nav className="mb-2 flex gap-1 border-b">
        <NavLink to={`/@${handle}/${id}/favourites`} className={cls}>
          Favourites
        </NavLink>
        <NavLink to={`/@${handle}/${id}/reblogs`} className={cls}>
          Boosts
        </NavLink>
      </nav>
      {error && <p className="text-destructive text-sm">{error}</p>}
      <div className="space-y-1">
        {accounts === null && !error && (
          <AccountListSkeleton />
        )}
        {accounts?.map((a) => (
          <AccountRow key={a.id} account={a} />
        ))}
        {accounts?.length === 0 && (
          <p className="text-muted-foreground text-sm">
            {boosts ? 'Nobody has boosted this yet.' : 'Nobody has favourited this yet.'}
          </p>
        )}
        <InfiniteScroll
          onLoadMore={feed.loadMore}
          loading={feed.loadingMore}
          done={feed.done}
          hasItems={!!accounts?.length}
        />
      </div>
    </div>
  )
}
