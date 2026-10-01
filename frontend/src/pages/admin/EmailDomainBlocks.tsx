import { useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createEmailDomainBlock,
  deleteEmailDomainBlock,
  listEmailDomainBlocks,
  type EmailDomainBlock,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { BLOCK_TABS } from './IpBlocks.tsx'

/** Sign-up attempts in the last week, from the block's daily history. */
function attempts(block: EmailDomainBlock): number {
  return block.history.reduce((sum, day) => sum + Number(day.accounts || 0), 0)
}

/**
 * Email domains sign-ups are refused from, or held for approval from. There is
 * no API to change a block once made — upstream has none either — so changing
 * one is removing it and adding it again.
 */
export default function EmailDomainBlocks() {
  const token = getToken()
  const feed = useInfinitePaginator<EmailDomainBlock>(
    () => listEmailDomainBlocks(token ?? ''),
    [token],
  )
  const [domain, setDomain] = useState('')
  const [withApproval, setWithApproval] = useState(false)
  const [saving, setSaving] = useState(false)

  const add = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || !domain.trim()) return
    setSaving(true)
    try {
      const block = await createEmailDomainBlock(token, {
        domain: domain.trim(),
        allow_with_approval: withApproval,
      })
      feed.mutate((items) => [block, ...items])
      setDomain('')
      setWithApproval(false)
      toast.success(`Blocked sign-ups from ${block.domain}.`)
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title="Email domains" permission="manage_blocks" sub={BLOCK_TABS}>
      <form onSubmit={add} className="mb-4 space-y-2 rounded-lg border p-3">
        <div className="flex gap-2">
          <Input
            aria-label="Email domain"
            placeholder="example.com"
            value={domain}
            onChange={(e) => setDomain(e.target.value)}
          />
          <Button type="submit" disabled={saving || !domain.trim()}>
            Block
          </Button>
        </div>
        <Label className="text-sm font-normal">
          <Checkbox
            checked={withApproval}
            onCheckedChange={(v) => setWithApproval(v === true)}
          />
          Allow sign-ups, but hold them for approval
        </Label>
      </form>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((b) => (
          <div key={b.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="truncate text-sm font-medium">{b.domain}</span>
                <Badge variant={b.allow_with_approval ? 'secondary' : 'destructive'}>
                  {b.allow_with_approval ? 'Needs approval' : 'Blocked'}
                </Badge>
              </div>
              <div className="text-muted-foreground text-xs">
                {attempts(b)} sign-up attempts this week · {formatDate(b.created_at)}
              </div>
            </div>
            <ConfirmButton
              size="xs"
              title={`Unblock ${b.domain}?`}
              description="People can sign up with addresses at this domain again."
              confirmLabel="Unblock"
              onConfirm={async () => {
                await deleteEmailDomainBlock(token ?? '', b.id)
                feed.mutate((items) => items.filter((x) => x.id !== b.id))
                toast.success(`Unblocked ${b.domain}.`)
              }}
            >
              Unblock
            </ConfirmButton>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No email domains are blocked.</p>
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
