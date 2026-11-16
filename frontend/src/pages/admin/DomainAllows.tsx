import { useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createDomainAllow,
  deleteDomainAllow,
  listDomainAllows,
  type DomainAllow,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/**
 * Domains allowed to federate. These only matter in Mastodon's limited
 * federation mode, where nothing federates unless it is listed here; on an
 * open server the list is kept but has no effect.
 */
export default function DomainAllows() {
  const token = getToken()
  const feed = useInfinitePaginator<DomainAllow>(
    () => listDomainAllows(token ?? ''),
    [token],
  )
  const [domain, setDomain] = useState('')
  const [saving, setSaving] = useState(false)

  const add = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || !domain.trim()) return
    setSaving(true)
    try {
      const allow = await createDomainAllow(token, domain.trim())
      feed.mutate((items) => [allow, ...items.filter((a) => a.id !== allow.id)])
      setDomain('')
      toast.success(t({ id: 'eunha.admin.domain_allows.allowed', defaultMessage: 'Allowed {domain}.' }, { domain: allow.domain }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title={t({ id: 'eunha.admin.domain_allows.domain_allows', defaultMessage: 'Domain allows' })} permission="manage_federation">
      <p className="text-muted-foreground mb-3 text-sm">
        {t({ id: 'eunha.admin.domain_allows.used_when_federation_is_limited_to', defaultMessage: 'Used when federation is limited to an allow list: only the domains here can federate with this server.' })}
      </p>
      <form onSubmit={add} className="mb-4 flex gap-2">
        <Input
          aria-label={t({ id: 'eunha.admin.domain_allows.domain_to_allow', defaultMessage: 'Domain to allow' })}
          placeholder="example.com"
          value={domain}
          onChange={(e) => setDomain(e.target.value)}
        />
        <Button type="submit" disabled={saving || !domain.trim()}>
          {t({ id: 'mastodon.admin.trends.allow', defaultMessage: 'Allow' })}
        </Button>
      </form>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <RowsSkeleton action />
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((a) => (
          <div key={a.id} className="flex items-center gap-2 p-2.5">
            <span className="min-w-0 flex-1 truncate text-sm font-medium">{a.domain}</span>
            <span className="text-muted-foreground hidden text-xs sm:inline">
              {formatDate(a.created_at)}
            </span>
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.admin.domain_allows.stop_allowing', defaultMessage: 'Stop allowing {domain}?' }, { domain: a.domain })}
              description={t({ id: 'eunha.admin.domain_allows.under_limited_federation_the_domain_can', defaultMessage: 'Under limited federation, the domain can no longer federate with this server.' })}
              confirmLabel={t({ id: 'tag.remove', defaultMessage: 'Remove' })}
              onConfirm={async () => {
                await deleteDomainAllow(token ?? '', a.id)
                feed.mutate((items) => items.filter((x) => x.id !== a.id))
                toast.success(t({ id: 'eunha.admin.domain_allows.removed', defaultMessage: 'Removed {domain}.' }, { domain: a.domain }))
              }}
            >
              {t({ id: 'tag.remove', defaultMessage: 'Remove' })}
            </ConfirmButton>
          </div>
        ))}
        {feed.items?.length === 0 && (
          <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.domain_allows.no_domains_are_allowed', defaultMessage: 'No domains are allowed.' })}</p>
        )}
      </div>
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
    </AdminLayout>
  )
}
