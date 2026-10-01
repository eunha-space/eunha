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
import { FEDERATION_TABS } from './DomainBlocks.tsx'

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
      toast.success(`Allowed ${allow.domain}.`)
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title="Domain allows" permission="manage_federation" sub={FEDERATION_TABS}>
      <p className="text-muted-foreground mb-3 text-sm">
        Used when federation is limited to an allow list: only the domains here can
        federate with this server.
      </p>
      <form onSubmit={add} className="mb-4 flex gap-2">
        <Input
          aria-label="Domain to allow"
          placeholder="example.com"
          value={domain}
          onChange={(e) => setDomain(e.target.value)}
        />
        <Button type="submit" disabled={saving || !domain.trim()}>
          Allow
        </Button>
      </form>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
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
              title={`Stop allowing ${a.domain}?`}
              description="Under limited federation, the domain can no longer federate with this server."
              confirmLabel="Remove"
              onConfirm={async () => {
                await deleteDomainAllow(token ?? '', a.id)
                feed.mutate((items) => items.filter((x) => x.id !== a.id))
                toast.success(`Removed ${a.domain}.`)
              }}
            >
              Remove
            </ConfirmButton>
          </div>
        ))}
        {feed.items?.length === 0 && (
          <p className="text-muted-foreground text-sm">No domains are allowed.</p>
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
