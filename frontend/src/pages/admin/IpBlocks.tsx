import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  createIpBlock,
  deleteIpBlock,
  listIpBlocks,
  updateIpBlock,
  type IpBlock,
  type IpBlockSeverity,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

export const BLOCK_TABS = [
  { to: '/admin/ip_blocks', get label() { return t({ id: 'mastodon.admin.ip_blocks.title', defaultMessage: 'IP rules' }) } },
  { to: '/admin/email_domain_blocks', get label() { return t({ id: 'eunha.admin.ip_blocks.email_domains', defaultMessage: 'Email domains' }) } },
  { to: '/admin/canonical_email_blocks', get label() { return t({ id: 'eunha.admin.ip_blocks.email_addresses', defaultMessage: 'Email addresses' }) } },
  { to: '/admin/username_blocks', get label() { return t({ id: 'eunha.admin.ip_blocks.usernames', defaultMessage: 'Usernames' }) } },
]

// Mastodon's labels for `IpBlock#severity`.
export const IP_SEVERITIES: Record<IpBlockSeverity, string> = {
  get sign_up_requires_approval() { return t({ id: 'mastodon.simple_form.labels.ip_block.severities.sign_up_requires_approval', defaultMessage: 'Limit sign-ups' }) },
  get sign_up_block() { return t({ id: 'mastodon.simple_form.labels.ip_block.severities.sign_up_block', defaultMessage: 'Block sign-ups' }) },
  get no_access() { return t({ id: 'mastodon.simple_form.labels.ip_block.severities.no_access', defaultMessage: 'Block access' }) },
}

// `IpBlock::EXPIRATION_DURATIONS` in seconds, as Rails counts them (a month is
// 30.436875 days); "0" is never, and is omitted from the request.
const EXPIRES_IN: Record<string, string> = {
  get '0'() { return t({ id: 'mastodon.invites.expires_in_prompt', defaultMessage: 'Never' }) },
  '86400': '1 day',
  '1209600': '2 weeks',
  '2629746': '1 month',
  '15778476': '6 months',
  '31556952': '1 year',
  '94670856': '3 years',
}

function IpBlockDialog({
  open,
  onOpenChange,
  editing,
  token,
  onSaved,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  editing: IpBlock | null
  token: string
  onSaved: (block: IpBlock, created: boolean) => void
}) {
  const [ip, setIp] = useState('')
  const [severity, setSeverity] = useState<IpBlockSeverity>('sign_up_requires_approval')
  const [comment, setComment] = useState('')
  const [expiresIn, setExpiresIn] = useState('0')
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (!open) return
    setIp(editing?.ip ?? '')
    setSeverity(editing?.severity ?? 'sign_up_requires_approval')
    setComment(editing?.comment ?? '')
    setExpiresIn('0')
  }, [open, editing])

  const save = async () => {
    setSaving(true)
    try {
      const params = {
        ip: ip.trim(),
        severity,
        comment: comment.trim(),
        expires_in: expiresIn === '0' ? undefined : Number(expiresIn),
      }
      const block = editing
        ? await updateIpBlock(token, editing.id, params)
        : await createIpBlock(token, params)
      onSaved(block, !editing)
      onOpenChange(false)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{editing ? t({ id: 'eunha.admin.ip_blocks.edit_ip_rule', defaultMessage: 'Edit IP rule' }) : t({ id: 'eunha.admin.ip_blocks.new_ip_rule', defaultMessage: 'New IP rule' })}</DialogTitle>
          <DialogDescription>
            {t({ id: 'eunha.admin.ip_blocks.an_address_or_a_cidr_range', defaultMessage: 'An address or a CIDR range, e.g. 192.0.2.0/24 or 2001:db8::/32.' })}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-1">
            <Label htmlFor="ip-block-ip">{t({ id: 'mastodon.admin.accounts.ip', defaultMessage: 'IP' })}</Label>
            <Input
              id="ip-block-ip"
              value={ip}
              placeholder="192.0.2.0/24"
              onChange={(e) => setIp(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label>{t({ id: 'mastodon.simple_form.labels.ip_block.severity', defaultMessage: 'Rule' })}</Label>
            <ChoiceSelect
              label={t({ id: 'mastodon.simple_form.labels.ip_block.severity', defaultMessage: 'Rule' })}
              value={severity}
              items={IP_SEVERITIES}
              onChange={setSeverity}
            />
          </div>
          <div className="space-y-1">
            <Label>{t({ id: 'mastodon.invites.table.expires_at', defaultMessage: 'Expires' })}</Label>
            <ChoiceSelect
              label={t({ id: 'mastodon.invites.table.expires_at', defaultMessage: 'Expires' })}
              value={expiresIn}
              items={EXPIRES_IN}
              onChange={setExpiresIn}
            />
            {editing?.expires_at && (
              <p className="text-muted-foreground text-xs">
                {t({ id: 'eunha.admin.ip_blocks.currently_expires', defaultMessage: 'Currently expires {date}.' }, { date: formatDate(editing.expires_at) })}
              </p>
            )}
          </div>
          <div className="space-y-1">
            <Label htmlFor="ip-block-comment">{t({ id: 'mastodon.simple_form.labels.ip_block.comment', defaultMessage: 'Comment' })}</Label>
            <Input
              id="ip-block-comment"
              value={comment}
              placeholder={t({ id: 'eunha.admin.ip_blocks.why_this_rule_exists', defaultMessage: 'Why this rule exists' })}
              onChange={(e) => setComment(e.target.value)}
            />
          </div>
        </div>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={saving} />}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
          </DialogClose>
          <Button disabled={saving || !ip.trim()} onClick={() => void save()}>
            {saving ? t({ id: 'eunha.common.saving', defaultMessage: 'Saving…' }) : t({ id: 'eunha.admin.ip_blocks.save_rule', defaultMessage: 'Save rule' })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

export default function IpBlocks() {
  const token = getToken()
  const feed = useInfinitePaginator<IpBlock>(() => listIpBlocks(token ?? ''), [token])
  const [dialog, setDialog] = useState<{ editing: IpBlock | null } | null>(null)

  const saved = (block: IpBlock, created: boolean) => {
    feed.mutate((items) =>
      items.some((b) => b.id === block.id)
        ? items.map((b) => (b.id === block.id ? block : b))
        : [block, ...items],
    )
    toast.success(created ? t({ id: 'eunha.admin.ip_blocks.added_a_rule_for', defaultMessage: 'Added a rule for {ip}.' }, { ip: block.ip }) : t({ id: 'eunha.admin.ip_blocks.updated', defaultMessage: 'Updated {ip}.' }, { ip: block.ip }))
  }

  return (
    <AdminLayout
      title="IP rules"
      permission="manage_blocks"
      sub={BLOCK_TABS}
      actions={
        <Button size="sm" onClick={() => setDialog({ editing: null })}>
          {t({ id: 'mastodon.admin.rules.add_new', defaultMessage: 'Add rule' })}
        </Button>
      }
    >
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <RowsSkeleton action />
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((b) => (
          <div key={b.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-sm">{b.ip}</span>
                <Badge variant={b.severity === 'no_access' ? 'destructive' : 'secondary'}>
                  {IP_SEVERITIES[b.severity] ?? b.severity}
                </Badge>
              </div>
              <div className="text-muted-foreground text-xs">
                {b.comment && <span>{b.comment} · </span>}
                {b.expires_at ? t({ id: 'eunha.invites.expires_on', defaultMessage: 'Expires {date}' }, { date: formatDate(b.expires_at) }) : t({ id: 'eunha.admin.ip_blocks.never_expires', defaultMessage: 'Never expires' })}
              </div>
            </div>
            <Button size="xs" variant="outline" onClick={() => setDialog({ editing: b })}>
              {t({ id: 'status.edit', defaultMessage: 'Edit' })}
            </Button>
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.admin.ip_blocks.remove_the_rule_for', defaultMessage: 'Remove the rule for {ip}?' }, { ip: b.ip })}
              description={t({ id: 'eunha.admin.ip_blocks.the_address_range_is_treated_like', defaultMessage: 'The address range is treated like any other again.' })}
              confirmLabel={t({ id: 'tag.remove', defaultMessage: 'Remove' })}
              onConfirm={async () => {
                await deleteIpBlock(token ?? '', b.id)
                feed.mutate((items) => items.filter((x) => x.id !== b.id))
                toast.success(t({ id: 'eunha.admin.ip_blocks.removed_the_rule_for', defaultMessage: 'Removed the rule for {ip}.' }, { ip: b.ip }))
              }}
            >
              {t({ id: 'tag.remove', defaultMessage: 'Remove' })}
            </ConfirmButton>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.ip_blocks.no_ip_rules', defaultMessage: 'No IP rules.' })}</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
      {token && (
        <IpBlockDialog
          open={dialog !== null}
          onOpenChange={(open) => !open && setDialog(null)}
          editing={dialog?.editing ?? null}
          token={token}
          onSaved={saved}
        />
      )}
    </AdminLayout>
  )
}
