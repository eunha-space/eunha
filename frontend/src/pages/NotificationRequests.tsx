import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import { beginLogin, getToken } from '../auth.ts'
import { restClient, type mastodon } from '../masto.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { AccountRow } from '@/components/account-row.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select.tsx'
import { errorMessage } from '@/lib/utils.ts'
import { AccountListSkeleton, LinesSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const choices = { get accept() { return t({ id: 'notification_requests.accept', defaultMessage: 'Accept' }) }, get filter() { return t({ id: 'eunha.notification_requests.send_to_requests', defaultMessage: 'Send to requests' }) }, get drop() { return t({ id: 'notifications.policy.drop', defaultMessage: 'Ignore' }) } }
const rules = {
  get forNotFollowing() { return t({ id: 'eunha.notification_requests.accounts_you_don_t_follow', defaultMessage: 'Accounts you don’t follow' }) },
  get forNotFollowers() { return t({ id: 'eunha.notification_requests.accounts_that_don_t_follow_you', defaultMessage: 'Accounts that don’t follow you' }) },
  get forNewAccounts() { return t({ id: 'notifications.policy.filter_new_accounts_title', defaultMessage: 'New accounts' }) },
  get forPrivateMentions() { return t({ id: 'column.direct', defaultMessage: 'Private mentions' }) },
  get forLimitedAccounts() { return t({ id: 'eunha.notification_requests.moderation_limited_accounts', defaultMessage: 'Moderation-limited accounts' }) },
  get forBots() { return t({ id: 'notifications.policy.filter_bots_title', defaultMessage: 'Bots' }) },
}
type Rule = keyof typeof rules
type Policy = Record<Rule, mastodon.v2.NotificationPolicyType>

function NotificationPolicy({ token }: { token: string }) {
  const [policy, setPolicy] = useState<Policy>()
  const [error, setError] = useState(false)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    let active = true
    restClient(token).v2.notifications.policy.fetch()
      .then((value) => { if (active) setPolicy({ forBots: 'accept', ...value }) })
      .catch(() => { if (active) setError(true) })
    return () => { active = false }
  }, [token])

  const change = async (rule: Rule, value: mastodon.v2.NotificationPolicyType) => {
    setSaving(true)
    try {
      const updated = await restClient(token).v2.notifications.policy.update({ [rule]: value })
      setPolicy({ forBots: 'accept', ...updated })
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-2 rounded-md border p-3">
      <h2 className="font-semibold">{t({ id: 'eunha.notification_requests.notification_policy', defaultMessage: 'Notification policy' })}</h2>
      {error && <p role="alert">{t({ id: 'eunha.notification_requests.could_not_load_your_notification_policy', defaultMessage: 'Could not load your notification policy.' })}</p>}
      {!policy && !error && <LinesSkeleton count={4} />}
      {policy && (Object.keys(rules) as Rule[]).map((rule) => (
        <div key={rule} className="flex flex-wrap items-center justify-between gap-2 text-sm">
          <span>{rules[rule]}</span>
          <Select items={choices} value={policy[rule]} disabled={saving}
            onValueChange={(value) => { if (value) void change(rule, value) }}>
            <SelectTrigger aria-label={rules[rule]}><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectGroup>
                {Object.entries(choices).map(([value, label]) => (
                  <SelectItem key={value} value={value}>{label}</SelectItem>
                ))}
              </SelectGroup>
            </SelectContent>
          </Select>
        </div>
      ))}
    </section>
  )
}

function RequestList({ token }: { token: string }) {
  const requests = restClient(token).v1.notifications.requests
  const feed = useInfinitePaginator(() => requests.list({ limit: 40 }), [token])
  const [busy, setBusy] = useState(false)

  const resolve = async (id: string, accept: boolean) => {
    setBusy(true)
    try {
      const request = requests.$select(id)
      if (accept) await request.accept()
      else await request.dismiss()
      feed.mutate((items) => items.filter((item) => item.id !== id))
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="space-y-2">
      <h2 className="font-semibold">{t({ id: 'eunha.notification_requests.pending_requests', defaultMessage: 'Pending requests' })}</h2>
      {feed.error && <p role="alert">{feed.error}</p>}
      {!feed.items && !feed.error && <AccountListSkeleton count={3} />}
      {feed.items?.length === 0 && <p>{t({ id: 'eunha.notification_requests.no_pending_requests', defaultMessage: 'No pending requests.' })}</p>}
      {feed.items?.map((request) => (
        <div key={request.id} className="space-y-2 rounded-md border p-3">
          <AccountRow account={request.account} />
          {request.lastStatus && <StatusCard status={request.lastStatus} token={token} />}
          <p className="text-muted-foreground text-sm">{t({ id: 'eunha.notification_requests.filtered_count', defaultMessage: '{count, plural, one {# filtered notification} other {# filtered notifications}}' }, { count: request.notificationsCount })}</p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void resolve(request.id, true)}>{t({ id: 'notification_requests.accept', defaultMessage: 'Accept' })}</Button>
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void resolve(request.id, false)}>{t({ id: 'dismissable_banner.dismiss', defaultMessage: 'Dismiss' })}</Button>
          </div>
        </div>
      ))}
      {feed.items && !feed.done && !feed.error && (
        <Button variant="outline" disabled={feed.loadingMore} onClick={feed.loadMore}>{t({ id: 'status.load_more', defaultMessage: 'Load more' })}</Button>
      )}
    </section>
  )
}

export default function NotificationRequests() {
  const token = getToken()
  return (
    <div className="page-frame space-y-3">
      <TopBar />
      <div className="flex items-center gap-2">
        <h1 className="min-w-0 flex-1 truncate text-xl font-bold">{t({ id: 'eunha.notification_requests.notification_requests', defaultMessage: 'Notification requests' })}</h1>
        <Button variant="outline" size="sm" render={<Link to="/notifications" />}>{t({ id: 'column.notifications', defaultMessage: 'Notifications' })}</Button>
      </div>
      {token ? <><RequestList token={token} /><NotificationPolicy token={token} /></>
        : <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>}
    </div>
  )
}
