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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

const STATES: Record<Appeal['state'], string> = {
  get pending() { return t({ id: 'account.pending', defaultMessage: 'Pending' }) },
  get approved() { return t({ id: 'mastodon.admin.trends.approved', defaultMessage: 'Approved' }) },
  get rejected() { return t({ id: 'mastodon.admin.trends.rejected', defaultMessage: 'Rejected' }) },
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
      toast.success(decision === 'approve' ? t({ id: 'eunha.admin.appeals.appeal_approved', defaultMessage: 'Appeal approved.' }) : t({ id: 'eunha.admin.appeals.appeal_rejected', defaultMessage: 'Appeal rejected.' }))
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.disputes.appeals.title', defaultMessage: 'Appeals' })} permission="manage_appeals">
      <div className="mb-3">
        <ChoiceSelect
          label={t({ id: 'mastodon.admin.follow_recommendations.status', defaultMessage: 'Status' })}
          value={status}
          items={STATES}
          className="w-full sm:w-48"
          onChange={(v) => setParams({ status: v }, { replace: true })}
        />
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <RowsSkeleton action />
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
              {(() => {
                const strike = (
                  <Link to={`/disputes/strikes/${a.strike.id}`}>
                    {t(
                      { id: 'eunha.admin.appeals.strike_on', defaultMessage: '{action} on {date}' },
                      { action: STRIKE_TITLES[a.strike.action], date: formatDate(a.strike.created_at) },
                    )}
                  </Link>
                )
                return a.strike.account ? (
                  <FormattedMessage
                    id='eunha.admin.appeals.appealing_by'
                    defaultMessage='Appealing {strike} by {username}'
                    values={{ strike, username: a.strike.account.username }}
                  />
                ) : (
                  <FormattedMessage
                    id='eunha.admin.appeals.appealing'
                    defaultMessage='Appealing {strike}'
                    values={{ strike }}
                  />
                )
              })()}
            </p>
            <blockquote className="border-l-2 pl-3 text-sm whitespace-pre-wrap">{a.text}</blockquote>
            <div className="text-muted-foreground text-xs">
              {t({ id: 'eunha.admin.appeals.submitted', defaultMessage: 'Submitted {date}' }, { date: formatDate(a.created_at) })}
            </div>
            {a.state === 'pending' && (
              <div className="flex gap-2">
                <Button size="sm" onClick={() => void decide(a, 'approve')}>
                  {t({ id: 'mastodon.disputes.strikes.approve_appeal', defaultMessage: 'Approve appeal' })}
                </Button>
                <Button size="sm" variant="destructive" onClick={() => void decide(a, 'reject')}>
                  {t({ id: 'mastodon.disputes.strikes.reject_appeal', defaultMessage: 'Reject appeal' })}
                </Button>
              </div>
            )}
          </article>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.appeals.no_appeals_to_show', defaultMessage: 'No appeals to show.' })}</p>
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
