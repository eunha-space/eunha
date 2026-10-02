import { useEffect, useState } from 'react'
import { Link, useParams, useSearchParams } from 'react-router-dom'

import {
  getAccount,
  listRelationships,
  type AdminAccount,
  type RelationshipFilters,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import {
  AccountStateBadges,
  AdminAccountLink,
  ChoiceSelect,
} from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'

type Relationship = NonNullable<RelationshipFilters['relationship']>
type Location = NonNullable<RelationshipFilters['location']> | 'all'
type Order = NonNullable<RelationshipFilters['order']>

// Mastodon's `relationships.*` filter labels.
const RELATIONSHIPS: Record<Relationship, string> = {
  following: 'Following',
  followed_by: 'Followers',
  mutual: 'Mutual',
  invited: 'Invited',
}
const LOCATIONS: Record<Location, string> = { all: 'All', local: 'Local', remote: 'Remote' }
const ORDERS: Record<Order, string> = { recent: 'Most recent', active: 'Most active' }

/**
 * Whom an account follows, who follows it, and whom it invited: Mastodon's
 * `Admin::RelationshipsController`.
 */
export default function Relationships() {
  const { id = '' } = useParams()
  const token = getToken()
  const [params, setParams] = useSearchParams()
  const relationship = (params.get('relationship') as Relationship | null) ?? 'following'
  const location = (params.get('location') as Location | null) ?? 'all'
  const order = (params.get('order') as Order | null) ?? 'recent'
  const [account, setAccount] = useState<AdminAccount | null>(null)
  const feed = useInfinitePaginator<AdminAccount>(
    () =>
      listRelationships(token ?? '', id, {
        relationship,
        location: location === 'all' ? undefined : location,
        order,
      }),
    [token, id, relationship, location, order],
  )

  useEffect(() => {
    if (!token) return
    getAccount(token, id).then(setAccount).catch(() => {})
  }, [token, id])

  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params)
    next.set(key, value)
    setParams(next, { replace: true })
  }

  return (
    <AdminLayout
      title={account ? `Relationships of @${account.account.acct}` : 'Relationships'}
      permission="manage_users"
    >
      <p className="mb-3 text-sm">
        <Link to={`/admin/accounts/${id}`}>Back to account</Link>
      </p>
      <div className="mb-3 grid gap-2 sm:grid-cols-3">
        <ChoiceSelect
          label="Relationship"
          value={relationship}
          items={RELATIONSHIPS}
          onChange={(v) => set('relationship', v)}
        />
        <ChoiceSelect
          label="Location"
          value={location}
          items={LOCATIONS}
          onChange={(v) => set('location', v)}
        />
        <ChoiceSelect label="Order" value={order} items={ORDERS} onChange={(v) => set('order', v)} />
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((a) => (
          <div key={a.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <AdminAccountLink account={a} />
            </div>
            <AccountStateBadges account={a} />
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No accounts.</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
    </AdminLayout>
  )
}
