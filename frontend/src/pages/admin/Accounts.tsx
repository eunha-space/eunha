import { useState, type FormEvent } from 'react'
import { Link, useSearchParams } from 'react-router-dom'

import { listAccounts, type AccountFilters, type AdminAccount } from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import {
  AccountStateBadges,
  AdminAccountLink,
  ChoiceSelect,
} from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

const ORIGINS = { get all() { return t({ id: 'eunha.admin.accounts.anywhere', defaultMessage: 'Anywhere' }) }, get local() { return t({ id: 'mastodon.admin.accounts.location.local', defaultMessage: 'Local' }) }, get remote() { return t({ id: 'mastodon.admin.accounts.location.remote', defaultMessage: 'Remote' }) } }
const STATUSES = {
  get all() { return t({ id: 'eunha.admin.accounts.any_status', defaultMessage: 'Any status' }) },
  get active() { return t({ id: 'mastodon.admin.accounts.moderation.active', defaultMessage: 'Active' }) },
  get pending() { return t({ id: 'mastodon.admin.accounts.pending', defaultMessage: 'Pending review' }) },
  sensitized: 'Force-sensitive',
  get silenced() { return t({ id: 'about.domain_blocks.silenced.title', defaultMessage: 'Limited' }) },
  get disabled() { return t({ id: 'mastodon.admin.accounts.disabled', defaultMessage: 'Frozen' }) },
  get suspended() { return t({ id: 'about.domain_blocks.suspended.title', defaultMessage: 'Suspended' }) },
}
const ROLES = { get all() { return t({ id: 'eunha.admin.accounts.any_role', defaultMessage: 'Any role' }) }, get staff() { return t({ id: 'eunha.admin.accounts.moderators_and_admins', defaultMessage: 'Moderators and admins' }) } }

// The text filters `GET /api/v2/admin/accounts` takes, in Mastodon's order.
const TEXT_FILTERS: { key: keyof AccountFilters; label: string; remoteOnly?: boolean }[] = [
  { key: 'username', get label() { return t({ id: 'mastodon.admin.accounts.username', defaultMessage: 'Username' }) } },
  { key: 'display_name', get label() { return t({ id: 'account_edit.display_name.title', defaultMessage: 'Display name' }) } },
  { key: 'by_domain', get label() { return t({ id: 'mastodon.admin.accounts.by_domain', defaultMessage: 'Domain' }) }, remoteOnly: true },
  { key: 'email', get label() { return t({ id: 'email_subscriptions.email', defaultMessage: 'Email' }) } },
  { key: 'ip', label: 'IP' },
]

function AccountRowItem({ account }: { account: AdminAccount }) {
  return (
    <div className={`hover:bg-muted/40 flex flex-wrap items-center gap-2 rounded-lg border p-2 ${
        account.suspended || account.account.suspended ||
        (account.domain === null && (account.approved === null || account.confirmed === false))
          ? 'opacity-60' : ''
      }`}>
      <div className="min-w-0 flex-1">
        <AdminAccountLink account={account} />
      </div>
      <AccountStateBadges account={account} />
      <div className="text-muted-foreground w-full text-xs sm:w-auto sm:text-right">
        {account.domain === null ? (
          <>
            {account.email && <span className="block truncate">{account.email}</span>}
            {account.ip && <span className="block">{account.ip}</span>}
          </>
        ) : (
          <span className="block">{account.domain}</span>
        )}
        <span className="block">
          <FormattedMessage
            id='eunha.admin.accounts.joined'
            defaultMessage='Joined {when}'
            values={{ when: <RelativeTime value={account.created_at} /> }}
          />
        </span>
      </div>
      {account.domain === null && account.approved === false && account.invite_request && (
        <p className="text-muted-foreground w-full text-sm italic">
          “{account.invite_request}”
        </p>
      )}
    </div>
  )
}

/**
 * Mastodon's account list for moderators, on `GET /api/v2/admin/accounts`.
 * The filters live in the query string, so a filtered list is a link that can
 * be shared with another moderator — and the dashboard's "pending" count links
 * straight to one.
 */
export default function Accounts() {
  const token = getToken()
  const [params, setParams] = useSearchParams()
  const origin = (params.get('origin') ?? 'all') as keyof typeof ORIGINS
  const status = (params.get('status') ?? 'all') as keyof typeof STATUSES
  const role = (params.get('permissions') ?? 'all') as keyof typeof ROLES
  const [draft, setDraft] = useState<Record<string, string>>(() =>
    Object.fromEntries(TEXT_FILTERS.map((f) => [f.key, params.get(f.key) ?? ''])),
  )

  const filters: AccountFilters = {
    origin: origin === 'all' ? undefined : origin,
    status: status === 'all' ? undefined : status,
    permissions: role === 'all' ? undefined : role,
  }
  for (const f of TEXT_FILTERS) {
    const v = params.get(f.key)
    if (v) (filters as Record<string, string>)[f.key] = v
  }
  const key = params.toString()

  const feed = useInfinitePaginator<AdminAccount>(
    () => listAccounts(token ?? '', filters),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [token, key],
  )

  const set = (name: string, value: string) => {
    const next = new URLSearchParams(params)
    if (value === 'all' || value === '') next.delete(name)
    else next.set(name, value)
    if (name === 'origin' && value !== 'remote') next.delete('by_domain')
    setParams(next)
  }

  const search = (e: FormEvent) => {
    e.preventDefault()
    const next = new URLSearchParams(params)
    for (const f of TEXT_FILTERS) {
      const v = draft[f.key]?.trim()
      if (v && (!f.remoteOnly || origin === 'remote')) next.set(f.key, v)
      else next.delete(f.key)
    }
    setParams(next)
  }

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.accounts.title', defaultMessage: 'Accounts' })} permission="manage_users">
      <div className="mb-3 space-y-2">
        <div className="grid gap-2 sm:grid-cols-3">
          <ChoiceSelect label={t({ id: 'mastodon.admin.accounts.location.title', defaultMessage: 'Location' })} value={origin} items={ORIGINS} onChange={(v) => set('origin', v)} />
          <ChoiceSelect label={t({ id: 'mastodon.admin.email_subscriptions.accounts.status', defaultMessage: 'Status' })} value={status} items={STATUSES} onChange={(v) => set('status', v)} />
          <ChoiceSelect label={t({ id: 'mastodon.admin.accounts.role', defaultMessage: 'Role' })} value={role} items={ROLES} onChange={(v) => set('permissions', v)} />
        </div>
        <form onSubmit={search} className="grid gap-2 sm:grid-cols-3">
          {TEXT_FILTERS.filter((f) => !f.remoteOnly || origin === 'remote').map((f) => (
            <Input
              key={f.key}
              aria-label={f.label}
              placeholder={f.label}
              value={draft[f.key] ?? ''}
              onChange={(e) => setDraft((d) => ({ ...d, [f.key]: e.target.value }))}
            />
          ))}
          <div className="flex gap-2">
            <Button type="submit">{t({ id: 'navigation_bar.search', defaultMessage: 'Search' })}</Button>
            {key && (
              <Button
                type="button"
                variant="ghost"
                render={<Link to="/admin/accounts" />}
                onClick={() => setDraft({})}
              >
                {t({ id: 'mastodon.admin.accounts.reset', defaultMessage: 'Reset' })}
              </Button>
            )}
          </div>
        </form>
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <AccountListSkeleton />
      )}
      <div className="space-y-2">
        {feed.items?.map((a) => <AccountRowItem key={a.id} account={a} />)}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.accounts.no_accounts_match', defaultMessage: 'No accounts match.' })}</p>
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
