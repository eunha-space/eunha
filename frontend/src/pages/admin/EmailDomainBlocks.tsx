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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

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
      toast.success(t({ id: 'eunha.admin.email_domain_blocks.blocked_sign_ups_from', defaultMessage: 'Blocked sign-ups from {domain}.' }, { domain: block.domain }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title={t({ id: 'eunha.admin.email_domain_blocks.email_domains', defaultMessage: 'Email domains' })} permission="manage_blocks" sub={BLOCK_TABS}>
      <form onSubmit={add} className="mb-4 space-y-2 rounded-lg border p-3">
        <div className="flex gap-2">
          <Input
            aria-label={t({ id: 'eunha.admin.email_domain_blocks.email_domain', defaultMessage: 'Email domain' })}
            placeholder="example.com"
            value={domain}
            onChange={(e) => setDomain(e.target.value)}
          />
          <Button type="submit" disabled={saving || !domain.trim()}>
            {t({ id: 'account.block_short', defaultMessage: 'Block' })}
          </Button>
        </div>
        <Label className="text-sm font-normal">
          <Checkbox
            checked={withApproval}
            onCheckedChange={(v) => setWithApproval(v === true)}
          />
          {t({ id: 'eunha.admin.email_domain_blocks.allow_sign_ups_but_hold_them', defaultMessage: 'Allow sign-ups, but hold them for approval' })}
        </Label>
      </form>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <RowsSkeleton action />
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((b) => (
          <div key={b.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="truncate text-sm font-medium">{b.domain}</span>
                <Badge variant={b.allow_with_approval ? 'secondary' : 'destructive'}>
                  {b.allow_with_approval ? t({ id: 'eunha.admin.email_domain_blocks.needs_approval', defaultMessage: 'Needs approval' }) : t({ id: 'account.blocked', defaultMessage: 'Blocked' })}
                </Badge>
              </div>
              <div className="text-muted-foreground text-xs">
                {t({ id: 'eunha.admin.email_domain_blocks.attempts', defaultMessage: '{count, plural, one {# sign-up attempt} other {# sign-up attempts}} this week' }, { count: attempts(b) })} · {formatDate(b.created_at)}
              </div>
            </div>
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.admin.email_domain_blocks.unblock', defaultMessage: 'Unblock {domain}?' }, { domain: b.domain })}
              description={t({ id: 'eunha.admin.email_domain_blocks.people_can_sign_up_with_addresses', defaultMessage: 'People can sign up with addresses at this domain again.' })}
              confirmLabel={t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
              onConfirm={async () => {
                await deleteEmailDomainBlock(token ?? '', b.id)
                feed.mutate((items) => items.filter((x) => x.id !== b.id))
                toast.success(t({ id: 'eunha.admin.email_domain_blocks.unblocked', defaultMessage: 'Unblocked {domain}.' }, { domain: b.domain }))
              }}
            >
              {t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
            </ConfirmButton>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.email_domain_blocks.no_email_domains_are_blocked', defaultMessage: 'No email domains are blocked.' })}</p>
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
