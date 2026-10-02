import { useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'

import {
  getActionLogFilters,
  listActionLogs,
  type ActionLog,
  type FilterChoice,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ActionLogEntry } from '@/components/admin/action-log-entry.tsx'
import { ChoiceSelect } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'

const ALL = 'all'

function choices(list: FilterChoice[]): Record<string, string> {
  const out: Record<string, string> = { [ALL]: 'All' }
  for (const c of list) out[c.key] = c.label
  return out
}

/**
 * The audit log: Mastodon's `Admin::ActionLogsController`, every moderation
 * action as one sentence, filtered by who acted and by what kind of action.
 * An account page links here filtered to what was done to that account.
 */
export default function ActionLogs() {
  const token = getToken()
  const [params, setParams] = useSearchParams()
  const accountId = params.get('account_id') ?? ''
  const actionType = params.get('action_type') ?? ''
  const targetAccountId = params.get('target_account_id') ?? ''
  const [filters, setFilters] = useState<{
    accounts: FilterChoice[]
    action_types: FilterChoice[]
  }>({ accounts: [], action_types: [] })

  useEffect(() => {
    if (!token) return
    getActionLogFilters(token).then(setFilters).catch(() => {})
  }, [token])

  const feed = useInfinitePaginator<ActionLog>(
    () =>
      listActionLogs(token ?? '', {
        account_id: accountId || undefined,
        action_type: actionType || undefined,
        target_account_id: targetAccountId || undefined,
      }),
    [token, accountId, actionType, targetAccountId],
  )

  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params)
    if (value && value !== ALL) next.set(key, value)
    else next.delete(key)
    setParams(next, { replace: true })
  }

  return (
    <AdminLayout title="Audit log" permission="view_audit_log">
      <div className="mb-3 grid gap-2 sm:grid-cols-2">
        <ChoiceSelect
          label="Filter by user"
          value={accountId || ALL}
          items={choices(filters.accounts)}
          onChange={(v) => set('account_id', v)}
        />
        <ChoiceSelect
          label="Filter by action"
          value={actionType || ALL}
          items={choices(filters.action_types)}
          onChange={(v) => set('action_type', v)}
        />
      </div>
      {targetAccountId && (
        <p className="text-muted-foreground mb-2 text-sm">
          Showing what was done to one account.{' '}
          <button
            type="button"
            className="text-primary underline"
            onClick={() => set('target_account_id', '')}
          >
            Show everything
          </button>
        </p>
      )}
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((log) => <ActionLogEntry key={log.id} log={log} />)}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No logs found.</p>
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
