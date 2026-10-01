import { useState, type ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { toast } from 'sonner'

import {
  listPublishers,
  listTrendingLinks,
  listTrendingStatuses,
  listTrendingTags,
  reviewTrend,
  type AdminTag,
  type History,
  type Publisher,
  type TrendKind,
  type TrendLink,
  type TrendStatus,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminStatus } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'

export const TREND_TABS = [
  { to: '/admin/trends/links', label: 'Links', end: true },
  { to: '/admin/trends/links/preview_card_providers', label: 'Publishers' },
  { to: '/admin/trends/statuses', label: 'Posts' },
  { to: '/admin/trends/tags', label: 'Hashtags' },
]

type Item = AdminTag | TrendLink | TrendStatus | Publisher

const KINDS: Record<string, { kind: TrendKind; title: string; open: (t: string) => AsyncIterable<Item[]> }> = {
  '/admin/trends/links': { kind: 'links', title: 'Trending links', open: listTrendingLinks },
  '/admin/trends/links/preview_card_providers': {
    kind: 'links/publishers',
    title: 'Publishers',
    open: listPublishers,
  },
  '/admin/trends/statuses': { kind: 'statuses', title: 'Trending posts', open: listTrendingStatuses },
  '/admin/trends/tags': { kind: 'tags', title: 'Trending hashtags', open: listTrendingTags },
}

function lastWeek(history: History[] | undefined): { uses: number; accounts: number } {
  return (history ?? []).reduce(
    (sum, day) => ({
      uses: sum.uses + Number(day.uses || 0),
      accounts: sum.accounts + Number(day.accounts || 0),
    }),
    { uses: 0, accounts: 0 },
  )
}

function hostOf(url: string): string {
  try {
    return new URL(url).host
  } catch {
    return url
  }
}

function Row({
  item,
  kind,
  token,
  onReviewed,
}: {
  item: Item
  kind: TrendKind
  token: string
  onReviewed: (item: Item) => void
}) {
  const [busy, setBusy] = useState(false)
  const review = async (decision: 'approve' | 'reject') => {
    setBusy(true)
    try {
      const updated = await reviewTrend<Item>(token, kind, item.id, decision)
      // Whatever came back replaces the row; a body without an id keeps the
      // old row, marked reviewed.
      onReviewed(
        updated && typeof updated === 'object' && 'id' in updated
          ? updated
          : ({ ...item, requires_review: false } as Item),
      )
      toast.success(decision === 'approve' ? 'Allowed to trend.' : 'Kept from trending.')
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  let body: ReactNode
  let trendable: boolean | null = null
  if (kind === 'tags') {
    const tag = item as AdminTag
    const week = lastWeek(tag.history)
    trendable = tag.trendable
    body = (
      <div>
        <Link to={`/tags/${tag.name}`} className="font-medium">
          #{tag.name}
        </Link>
        <p className="text-muted-foreground text-xs">
          {week.accounts} people, {week.uses} posts this week
        </p>
      </div>
    )
  } else if (kind === 'links') {
    const link = item as TrendLink
    const week = lastWeek(link.history)
    body = (
      <div className="flex gap-3">
        {link.image && (
          <img src={link.image} alt="" className="size-14 shrink-0 rounded object-cover" />
        )}
        <div className="min-w-0">
          <a href={link.url} target="_blank" rel="noreferrer" className="font-medium">
            {link.title || link.url}
          </a>
          <p className="text-muted-foreground truncate text-xs">
            {link.provider_name || hostOf(link.url)} · {week.accounts} people shared
            this week
          </p>
        </div>
      </div>
    )
  } else if (kind === 'links/publishers') {
    const p = item as Publisher
    trendable = p.trendable
    body = <span className="font-medium">{p.domain}</span>
  } else {
    body = <AdminStatus status={item as TrendStatus} />
  }

  const requiresReview = 'requires_review' in item && item.requires_review

  return (
    <div className="space-y-2 rounded-lg border p-3">
      {body}
      <div className="flex flex-wrap items-center gap-2">
        {requiresReview && <Badge variant="secondary">Pending review</Badge>}
        {trendable === true && !requiresReview && <Badge variant="outline">Allowed</Badge>}
        {trendable === false && !requiresReview && <Badge variant="outline">Not allowed</Badge>}
        <span className="flex-1" />
        <Button size="xs" variant="outline" disabled={busy} onClick={() => void review('approve')}>
          Allow
        </Button>
        <Button size="xs" variant="outline" disabled={busy} onClick={() => void review('reject')}>
          Disallow
        </Button>
      </div>
    </div>
  )
}

/**
 * What is trending, for review: Mastodon's admin trends pages on
 * `/api/v1/admin/trends/*`. Nothing new trends publicly until a moderator
 * allows it — or its publisher, for links — and disallowing keeps it off for
 * good.
 */
export default function Trends() {
  const token = getToken()
  const { pathname } = useLocation()
  const config = KINDS[pathname] ?? KINDS['/admin/trends/links']
  const feed = useInfinitePaginator<Item>(() => config.open(token ?? ''), [token, config.kind])

  return (
    <AdminLayout title={config.title} permission="manage_taxonomies" sub={TREND_TABS}>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className="space-y-2">
        {token &&
          feed.items?.map((item) => (
            <Row
              key={item.id}
              item={item}
              kind={config.kind}
              token={token}
              onReviewed={(updated) =>
                feed.mutate((items) => items.map((i) => (i.id === updated.id ? updated : i)))
              }
            />
          ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">Nothing is trending right now.</p>
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
