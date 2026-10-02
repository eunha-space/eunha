import { Link, useSearchParams } from 'react-router-dom'
import { toast } from 'sonner'

import { decideAppeal, listAppeals, type AdminAppeal, type Appeal } from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminAccountLink, ChoiceSelect, formatDate } from '@/components/admin/admin-common.tsx'
import { STRIKE_TITLES } from '@/components/admin/strike-card.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'

const STATES: Record<Appeal['state'], string> = {
  pending: 'Pending',
  approved: 'Approved',
  rejected: 'Rejected',
}

/**
 * Appeals against strikes: Mastodon's `Admin::Disputes::AppealsController`,
 * pending ones first. Approving undoes what the strike did, as far as it can
 * be undone; rejecting leaves it standing. The account is mailed either way.
 */
export default function Appeals() {
  const token = getToken()
  const [params, setParams] = useSearchParams()
  const status = (params.get('status') as Appeal['state'] | null) ?? 'pending'
  const feed = useInfinitePaginator<AdminAppeal>(
    () => listAppeals(token ?? '', status),
    [token, status],
  )

  const decide = async (appeal: AdminAppeal, decision: 'approve' | 'reject') => {
    try {
      await decideAppeal(token ?? '', appeal.id, decision)
      feed.mutate((items) => items.filter((a) => a.id !== appeal.id))
      toast.success(decision === 'approve' ? 'Appeal approved.' : 'Appeal rejected.')
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title="Appeals" permission="manage_appeals">
      <div className="mb-3">
        <ChoiceSelect
          label="Status"
          value={status}
          items={STATES}
          className="w-full sm:w-48"
          onChange={(v) => setParams({ status: v }, { replace: true })}
        />
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className="space-y-2">
        {feed.items?.map((a) => (
          <article key={a.id} className="space-y-2 rounded-lg border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <div className="min-w-0 flex-1">
                {a.account && <AdminAccountLink account={a.account} />}
              </div>
              <Badge variant={a.state === 'pending' ? 'secondary' : 'outline'}>
                {STATES[a.state]}
              </Badge>
            </div>
            <p className="text-sm">
              Appealing{' '}
              <Link to={`/disputes/strikes/${a.strike.id}`}>
                {STRIKE_TITLES[a.strike.action]} on {formatDate(a.strike.created_at)}
              </Link>
              {a.strike.account && <> by {a.strike.account.username}</>}
            </p>
            <blockquote className="border-l-2 pl-3 text-sm whitespace-pre-wrap">{a.text}</blockquote>
            <div className="text-muted-foreground text-xs">
              Submitted {formatDate(a.created_at)}
            </div>
            {a.state === 'pending' && (
              <div className="flex gap-2">
                <Button size="sm" onClick={() => void decide(a, 'approve')}>
                  Approve appeal
                </Button>
                <Button size="sm" variant="destructive" onClick={() => void decide(a, 'reject')}>
                  Reject appeal
                </Button>
              </div>
            )}
          </article>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No appeals to show.</p>
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
