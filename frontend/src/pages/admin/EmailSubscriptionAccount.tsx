import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  deleteEmailSubscriber,
  getEmailSubscriptionAccount,
  listEmailSubscribers,
  setEmailSubscriptionAccount,
  type EmailSubscriber,
  type EmailSubscriptionAccount as Entry,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import {
  AdminAccountLink,
  ConfirmButton,
  formatDate,
} from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Button } from '@/components/ui/button.tsx'
import { EmailSubscriptionStatusBadge } from './EmailSubscriptions.tsx'

/**
 * One account's mailing list: Mastodon's
 * `/admin/email_subscriptions/accounts/:id`, with the switch for the account's
 * own setting and the subscribers to remove one by one.
 */
export default function EmailSubscriptionAccount() {
  const { id = '' } = useParams()
  const token = getToken()
  const [entry, setEntry] = useState<Entry | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const feed = useInfinitePaginator<EmailSubscriber>(
    () => listEmailSubscribers(token ?? '', id),
    [token, id],
  )

  useEffect(() => {
    if (!token) return
    getEmailSubscriptionAccount(token, id)
      .then(setEntry)
      .catch((e) => setError(errorMessage(e)))
  }, [token, id])

  const name = entry ? entry.account.display_name || entry.account.username : ''

  const enable = async () => {
    if (!token) return
    setBusy(true)
    try {
      setEntry(await setEmailSubscriptionAccount(token, id, true))
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <AdminLayout
      title={entry ? `Email newsletters of ${name}` : 'Email newsletters'}
      permission="manage_settings"
    >
      <p className="mb-3 text-sm">
        <Link to="/admin/email_subscriptions">← Email newsletters</Link>
      </p>
      <AdminError error={error ?? feed.error} />
      {entry && token && (
        <div className="mb-4 space-y-3 rounded-lg border p-4">
          <div className="flex flex-wrap items-center gap-2">
            <div className="min-w-0 flex-1">
              <AdminAccountLink account={entry.account} />
            </div>
            {entry.status === 'active' ? (
              <ConfirmButton
                title="Disable feature"
                description={`Disable email newsletters for ${name}? Email updates will no longer be sent for this account. The user will still be able to re-enable the feature in their account settings. To permanently remove access to this feature, edit the account’s role.`}
                confirmLabel="Disable feature"
                onConfirm={async () => {
                  setEntry(await setEmailSubscriptionAccount(token, id, false))
                }}
              >
                Disable feature
              </ConfirmButton>
            ) : (
              entry.status === 'disabled' && (
                <Button size="sm" variant="outline" disabled={busy} onClick={enable}>
                  Enable feature
                </Button>
              )
            )}
          </div>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            <dt className="text-muted-foreground">Status</dt>
            <dd>
              <EmailSubscriptionStatusBadge status={entry.status} />
            </dd>
            <dt className="text-muted-foreground">Subscribers</dt>
            <dd>{entry.subscribers}</dd>
            <dt className="text-muted-foreground">Last email</dt>
            <dd>{entry.last_status_at ? formatDate(entry.last_status_at) : '–'}</dd>
          </dl>
          {entry.status === 'disabled' || entry.status === 'inactive' ? (
            <p className="text-muted-foreground text-sm">
              The feature was disabled and emails are no longer being sent to this list.
            </p>
          ) : entry.status === 'no_access' ? (
            <p className="text-muted-foreground text-sm">
              This account no longer has the permissions required to enable the feature.
              Change this in its role.
            </p>
          ) : null}
          <p className="text-muted-foreground text-xs">
            Subscribers have only consented to receiving posts via email. Do not use this list
            for other purposes.
          </p>
        </div>
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((s) => (
          <div key={s.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <div className="truncate text-sm font-medium">{s.email}</div>
              <div className="text-muted-foreground text-xs">
                Signed up {formatDate(s.created_at)}
                {s.confirmed_at ? '' : ' · unconfirmed'}
              </div>
            </div>
            {token && (
              <ConfirmButton
                size="xs"
                title="Remove subscriber?"
                description={`${s.email} will no longer receive emails from ${name}. This action cannot be undone.`}
                confirmLabel="Remove"
                onConfirm={async () => {
                  await deleteEmailSubscriber(token, s.id)
                  feed.mutate((items) => items.filter((x) => x.id !== s.id))
                  setEntry((e) => (e ? { ...e, subscribers: e.subscribers - 1 } : e))
                }}
              >
                Remove
              </ConfirmButton>
            )}
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">Nobody has subscribed to this account yet.</p>
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
