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
import { AccountListSkeleton, SectionSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

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
      title={entry ? t({ id: 'mastodon.admin.email_subscriptions.accounts.show.title', defaultMessage: 'Email newsletters of {name}' }, { name }) : t({ id: 'mastodon.admin.email_subscriptions.index.title', defaultMessage: 'Email newsletters' })}
      permission="manage_settings"
      actions={
        <Button size="sm" variant="outline" render={<Link to="/admin/email_subscriptions" />}>
          {t({ id: 'eunha.admin.email_subscription_account.email_newsletters_2', defaultMessage: '← Email newsletters' })}
        </Button>
      }
    >
      <AdminError error={error ?? feed.error} />
      {!entry && !error && <SectionSkeleton />}
      {entry && token && (
        <div className="mb-4 space-y-3 rounded-lg border p-4">
          <div className="flex flex-wrap items-center gap-2">
            <div className="min-w-0 flex-1">
              <AdminAccountLink account={entry.account} />
            </div>
            {entry.status === 'active' ? (
              <ConfirmButton
                title={t({ id: 'mastodon.admin.email_subscriptions.accounts.show.disable_feature', defaultMessage: 'Disable feature' })}
                description={t({ id: 'eunha.admin.email_subscription_account.disable_email_newsletters_for_email_updates', defaultMessage: 'Disable email newsletters for {name}? Email updates will no longer be sent for this account. The user will still be able to re-enable the feature in their account settings. To permanently remove access to this feature, edit the account’s role.' }, { name })}
                confirmLabel={t({ id: 'mastodon.admin.email_subscriptions.accounts.show.disable_feature', defaultMessage: 'Disable feature' })}
                onConfirm={async () => {
                  setEntry(await setEmailSubscriptionAccount(token, id, false))
                }}
              >
                {t({ id: 'mastodon.admin.email_subscriptions.accounts.show.disable_feature', defaultMessage: 'Disable feature' })}
              </ConfirmButton>
            ) : (
              entry.status === 'disabled' && (
                <Button size="sm" variant="outline" disabled={busy} onClick={enable}>
                  {t({ id: 'mastodon.admin.email_subscriptions.accounts.show.enable_feature', defaultMessage: 'Enable feature' })}
                </Button>
              )
            )}
          </div>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            <dt className="text-muted-foreground">{t({ id: 'mastodon.admin.follow_recommendations.status', defaultMessage: 'Status' })}</dt>
            <dd>
              <EmailSubscriptionStatusBadge status={entry.status} />
            </dd>
            <dt className="text-muted-foreground">{t({ id: 'mastodon.admin.email_subscriptions.accounts.subscribers', defaultMessage: 'Subscribers' })}</dt>
            <dd>{entry.subscribers}</dd>
            <dt className="text-muted-foreground">{t({ id: 'mastodon.admin.email_subscriptions.accounts.last_email', defaultMessage: 'Last email' })}</dt>
            <dd>{entry.last_status_at ? formatDate(entry.last_status_at) : '–'}</dd>
          </dl>
          {entry.status === 'disabled' || entry.status === 'inactive' ? (
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.admin.email_subscription_account.the_feature_was_disabled_and_emails', defaultMessage: 'The feature was disabled and emails are no longer being sent to this list.' })}
            </p>
          ) : entry.status === 'no_access' ? (
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.admin.email_subscription_account.this_account_no_longer_has_the', defaultMessage: 'This account no longer has the permissions required to enable the feature. Change this in its role.' })}
            </p>
          ) : null}
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.admin.email_subscription_account.subscribers_have_only_consented_to_receiving', defaultMessage: 'Subscribers have only consented to receiving posts via email. Do not use this list for other purposes.' })}
          </p>
        </div>
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((s) => (
          <div key={s.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <div className="truncate text-sm font-medium">{s.email}</div>
              <div className="text-muted-foreground text-xs">
                {t({ id: 'eunha.admin.email_subscription_account.signed_up', defaultMessage: 'Signed up {date}' }, { date: formatDate(s.created_at) })}
                {s.confirmed_at ? '' : ` · ${t({ id: 'eunha.admin.email_subscription_account.unconfirmed', defaultMessage: 'unconfirmed' })}`}
              </div>
            </div>
            {token && (
              <ConfirmButton
                size="xs"
                title={t({ id: 'eunha.admin.email_subscription_account.remove_subscriber', defaultMessage: 'Remove subscriber?' })}
                description={t({ id: 'eunha.admin.email_subscription_account.will_no_longer_receive_emails_from', defaultMessage: '{email} will no longer receive emails from {name}. This action cannot be undone.' }, { email: s.email, name })}
                confirmLabel={t({ id: 'tag.remove', defaultMessage: 'Remove' })}
                onConfirm={async () => {
                  await deleteEmailSubscriber(token, s.id)
                  feed.mutate((items) => items.filter((x) => x.id !== s.id))
                  setEntry((e) => (e ? { ...e, subscribers: e.subscribers - 1 } : e))
                }}
              >
                {t({ id: 'tag.remove', defaultMessage: 'Remove' })}
              </ConfirmButton>
            )}
          </div>
        ))}
      </div>
      {feed.items === null && !feed.error && <AccountListSkeleton count={3} />}
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'mastodon.admin.email_subscriptions.accounts.show.empty.hint', defaultMessage: 'Nobody has subscribed to this account yet.' })}</p>
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
