import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  batchAccountStatuses,
  getAccount,
  listAccountStatuses,
  type AdminAccount,
  type Status,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminStatus } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Label } from '@/components/ui/label.tsx'

/**
 * An account's public and unlisted posts as a moderator sees them: Mastodon's
 * `Admin::StatusesController#index`. Picked posts go into a new report, or
 * into the report given as `?report_id=`, as its batch action does.
 */
export default function AccountStatuses() {
  const { id = '' } = useParams()
  const navigate = useNavigate()
  const token = getToken()
  const [params, setParams] = useSearchParams()
  const media = params.get('media') === '1'
  const reportId = params.get('report_id') ?? undefined
  const [account, setAccount] = useState<AdminAccount | null>(null)
  const [selected, setSelected] = useState<string[]>([])
  const [busy, setBusy] = useState(false)
  const feed = useInfinitePaginator<Status>(
    () => listAccountStatuses(token ?? '', id, media),
    [token, id, media],
  )

  useEffect(() => {
    if (!token) return
    getAccount(token, id).then(setAccount).catch(() => {})
  }, [token, id])

  const report = async () => {
    if (!token || busy || selected.length === 0) return
    setBusy(true)
    try {
      const result = await batchAccountStatuses(token, id, {
        type: 'report',
        status_ids: selected,
        report_id: reportId,
      })
      toast.success('Added to the report.')
      if (result.id) navigate(`/admin/reports/${result.id}`)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const acct = account?.account.acct

  return (
    <AdminLayout
      title={acct ? `Posts by @${acct}` : 'Posts'}
      permission="manage_users"
      actions={
        <Button size="sm" disabled={busy || selected.length === 0} onClick={() => void report()}>
          {reportId ? `Add to report #${reportId}` : 'Report'} ({selected.length})
        </Button>
      }
    >
      <div className="mb-3 flex flex-wrap items-center gap-3 text-sm">
        <Link to={`/admin/accounts/${id}`}>Back to account</Link>
        {reportId && <Link to={`/admin/reports/${reportId}`}>Back to report</Link>}
        <Label className="font-normal">
          <Checkbox
            checked={media}
            onCheckedChange={(v) => {
              const next = new URLSearchParams(params)
              if (v === true) next.set('media', '1')
              else next.delete('media')
              setParams(next, { replace: true })
            }}
          />
          With media
        </Label>
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className="space-y-2">
        {feed.items?.map((s) => (
          <div key={s.id} className="flex items-start gap-2">
            <Checkbox
              className="mt-3"
              aria-label="Select post"
              checked={selected.includes(s.id)}
              onCheckedChange={(v) =>
                setSelected((ids) => (v === true ? [...ids, s.id] : ids.filter((x) => x !== s.id)))
              }
            />
            <div className="min-w-0 flex-1 space-y-1">
              <AdminStatus status={s} />
              <Link className="text-xs" to={`/admin/accounts/${id}/statuses/${s.id}`}>
                Open
              </Link>
            </div>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && <p className="text-muted-foreground text-sm">No posts.</p>}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
    </AdminLayout>
  )
}
