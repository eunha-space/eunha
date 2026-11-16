import { useEffect, useState } from 'react'
import { Link, useParams, useSearchParams } from 'react-router-dom'
import { Button } from '@/components/ui/button.tsx'

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
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

type Relationship = NonNullable<RelationshipFilters['relationship']>
type Location = NonNullable<RelationshipFilters['location']> | 'all'
type Order = NonNullable<RelationshipFilters['order']>

// Mastodon's `relationships.*` filter labels.
const RELATIONSHIPS: Record<Relationship, string> = {
  get following() { return t({ id: 'account.following', defaultMessage: 'Following' }) },
  get followed_by() { return t({ id: 'account.followers', defaultMessage: 'Followers' }) },
  get mutual() { return t({ id: 'mastodon.relationships.mutual', defaultMessage: 'Mutual' }) },
  get invited() { return t({ id: 'mastodon.relationships.invited', defaultMessage: 'Invited' }) },
}
const LOCATIONS: Record<Location, string> = { get all() { return t({ id: 'notifications.filter.all', defaultMessage: 'All' }) }, get local() { return t({ id: 'mastodon.admin.accounts.location.local', defaultMessage: 'Local' }) }, get remote() { return t({ id: 'mastodon.admin.accounts.location.remote', defaultMessage: 'Remote' }) } }
const ORDERS: Record<Order, string> = { get recent() { return t({ id: 'mastodon.relationships.most_recent', defaultMessage: 'Most recent' }) }, get active() { return t({ id: 'eunha.admin.relationships.most_active', defaultMessage: 'Most active' }) } }

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
      title={account ? t({ id: 'eunha.admin.relationships.relationships_of', defaultMessage: 'Relationships of @{acct}' }, { acct: account.account.acct }) : t({ id: 'navigation_bar.relationships', defaultMessage: 'Relationships' })}
      permission="manage_users"
      actions={
        <Button size="sm" variant="outline" render={<Link to={`/admin/accounts/${id}`} />}>
          {t({ id: 'eunha.admin.relationships.back_to_account', defaultMessage: 'Back to account' })}
        </Button>
      }
    >
      <div className="mb-3 grid gap-2 sm:grid-cols-3">
        <ChoiceSelect
          label={t({ id: 'mastodon.relationships.relationship', defaultMessage: 'Relationship' })}
          value={relationship}
          items={RELATIONSHIPS}
          onChange={(v) => set('relationship', v)}
        />
        <ChoiceSelect
          label={t({ id: 'mastodon.admin.accounts.location.title', defaultMessage: 'Location' })}
          value={location}
          items={LOCATIONS}
          onChange={(v) => set('location', v)}
        />
        <ChoiceSelect label={t({ id: 'eunha.admin.relationships.order', defaultMessage: 'Order' })} value={order} items={ORDERS} onChange={(v) => set('order', v)} />
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <AccountListSkeleton />
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
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.relationships.no_accounts', defaultMessage: 'No accounts.' })}</p>
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
