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

const choices = { accept: 'Accept', filter: 'Send to requests', drop: 'Ignore' }
const rules = {
  forNotFollowing: 'Accounts you don’t follow',
  forNotFollowers: 'Accounts that don’t follow you',
  forNewAccounts: 'New accounts',
  forPrivateMentions: 'Private mentions',
  forLimitedAccounts: 'Moderation-limited accounts',
  forBots: 'Bots',
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
      <h2 className="font-semibold">Notification policy</h2>
      {error && <p role="alert">Could not load your notification policy.</p>}
      {!policy && !error && <p>Loading…</p>}
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
      <h2 className="font-semibold">Pending requests</h2>
      {feed.error && <p role="alert">{feed.error}</p>}
      {!feed.items && !feed.error && <p>Loading…</p>}
      {feed.items?.length === 0 && <p>No pending requests.</p>}
      {feed.items?.map((request) => (
        <div key={request.id} className="space-y-2 rounded-md border p-3">
          <AccountRow account={request.account} />
          {request.lastStatus && <StatusCard status={request.lastStatus} token={token} />}
          <p className="text-muted-foreground text-sm">{request.notificationsCount} filtered notifications</p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void resolve(request.id, true)}>Accept</Button>
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void resolve(request.id, false)}>Dismiss</Button>
          </div>
        </div>
      ))}
      {feed.items && !feed.done && !feed.error && (
        <Button variant="outline" disabled={feed.loadingMore} onClick={feed.loadMore}>Load more</Button>
      )}
    </section>
  )
}

export default function NotificationRequests() {
  const token = getToken()
  return (
    <div className="page-frame space-y-3">
      <TopBar />
      <Link to="/notifications">Notifications</Link>
      <h1 className="text-xl font-bold">Notification requests</h1>
      {token ? <><RequestList token={token} /><NotificationPolicy token={token} /></>
        : <Button onClick={() => void beginLogin()}>Sign in</Button>}
    </div>
  )
}
