import { Link, useSearchParams } from 'react-router-dom'
import { Forward, MessageSquare } from 'lucide-react'

import { listReports, publicAccount, type AdminReport } from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminAccountLink } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { cn } from '@/lib/utils.ts'

export const CATEGORY_LABELS: Record<AdminReport['category'], string> = {
  spam: 'Spam',
  legal: 'Legal',
  violation: 'Rule violation',
  other: 'Other',
}

function ReportRow({ report }: { report: AdminReport }) {
  const target = publicAccount(report.target_account)
  const reporter = publicAccount(report.account)
  return (
    <div className="hover:bg-muted/40 space-y-2 rounded-lg border p-3">
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <AdminAccountLink account={report.target_account} />
        </div>
        <Link
          to={`/admin/reports/${report.id}`}
          className="text-muted-foreground shrink-0 text-xs no-underline hover:underline"
        >
          #{report.id} · <RelativeTime value={report.created_at} />
        </Link>
      </div>
      <Link to={`/admin/reports/${report.id}`} className="block space-y-1.5 no-underline">
        <div className="flex flex-wrap items-center gap-1.5">
          <Badge variant={report.category === 'other' ? 'outline' : 'secondary'}>
            {CATEGORY_LABELS[report.category] ?? report.category}
          </Badge>
          {report.statuses.length > 0 && (
            <Badge variant="outline">
              <MessageSquare /> {report.statuses.length}
            </Badge>
          )}
          {report.forwarded && (
            <Badge variant="outline">
              <Forward /> Forwarded
            </Badge>
          )}
          {report.action_taken && <Badge variant="outline">Resolved</Badge>}
          <span className="text-muted-foreground text-xs">
            by @{reporter.acct}
            {report.assigned_account &&
              ` · assigned to @${publicAccount(report.assigned_account).acct}`}
          </span>
        </div>
        {report.comment && (
          <p className="text-foreground line-clamp-2 text-sm">{report.comment}</p>
        )}
        <span className="sr-only">Open report on @{target.acct}</span>
      </Link>
    </div>
  )
}

const pill =
  'rounded-full px-3 py-1 text-sm text-muted-foreground no-underline hover:bg-muted/60 hover:text-foreground'

/**
 * Mastodon's report queue: unresolved first, newest first, filterable down to
 * one reporter or one reported account — which is how an account's page links
 * here.
 */
export default function Reports() {
  const token = getToken()
  const [params] = useSearchParams()
  const resolved = params.get('resolved') === 'true'
  const accountId = params.get('account_id') ?? undefined
  const targetAccountId = params.get('target_account_id') ?? undefined

  const feed = useInfinitePaginator<AdminReport>(
    () =>
      listReports(token ?? '', {
        resolved: resolved || undefined,
        account_id: accountId,
        target_account_id: targetAccountId,
      }),
    [token, resolved, accountId, targetAccountId],
  )

  const withFilter = (next: Record<string, string | undefined>) => {
    const search = new URLSearchParams(params)
    for (const [k, v] of Object.entries(next)) {
      if (v === undefined) search.delete(k)
      else search.set(k, v)
    }
    const s = search.toString()
    return s ? `/admin/reports?${s}` : '/admin/reports'
  }

  return (
    <AdminLayout title="Reports" permission="manage_reports">
      <div className="mb-3 flex flex-wrap items-center gap-1">
        <Link
          to={withFilter({ resolved: undefined })}
          className={cn(pill, !resolved && 'bg-muted text-foreground font-medium')}
        >
          Unresolved
        </Link>
        <Link
          to={withFilter({ resolved: 'true' })}
          className={cn(pill, resolved && 'bg-muted text-foreground font-medium')}
        >
          Resolved
        </Link>
        {(accountId || targetAccountId) && (
          <Link
            to={withFilter({ account_id: undefined, target_account_id: undefined })}
            className={cn(pill, 'ml-auto')}
          >
            {targetAccountId ? 'About one account' : 'By one account'} · Clear
          </Link>
        )}
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className="space-y-2">
        {feed.items?.map((r) => <ReportRow key={r.id} report={r} />)}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          {resolved ? 'No resolved reports.' : 'Nothing to review. All reports are resolved.'}
        </p>
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
