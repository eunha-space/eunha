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

export const BLOCK_TABS = [
  { to: '/admin/ip_blocks', label: 'IP rules' },
  { to: '/admin/email_domain_blocks', label: 'Email domains' },
  { to: '/admin/canonical_email_blocks', label: 'Email addresses' },
]

// Mastodon's labels for `IpBlock#severity`.
export const IP_SEVERITIES: Record<IpBlockSeverity, string> = {
  sign_up_requires_approval: 'Limit sign-ups',
  sign_up_block: 'Block sign-ups',
  no_access: 'Block access',
}

// `IpBlock::EXPIRATION_DURATIONS` in seconds, as Rails counts them (a month is
// 30.436875 days); "0" is never, and is omitted from the request.
const EXPIRES_IN: Record<string, string> = {
  '0': 'Never',
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
          <DialogTitle>{editing ? 'Edit IP rule' : 'New IP rule'}</DialogTitle>
          <DialogDescription>
            An address or a CIDR range, e.g. 192.0.2.0/24 or 2001:db8::/32.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-1">
            <Label htmlFor="ip-block-ip">IP</Label>
            <Input
              id="ip-block-ip"
              value={ip}
              placeholder="192.0.2.0/24"
              onChange={(e) => setIp(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label>Rule</Label>
            <ChoiceSelect
              label="Rule"
              value={severity}
              items={IP_SEVERITIES}
              onChange={setSeverity}
            />
          </div>
          <div className="space-y-1">
            <Label>Expires</Label>
            <ChoiceSelect
              label="Expires"
              value={expiresIn}
              items={EXPIRES_IN}
              onChange={setExpiresIn}
            />
            {editing?.expires_at && (
              <p className="text-muted-foreground text-xs">
                Currently expires {formatDate(editing.expires_at)}.
              </p>
            )}
          </div>
          <div className="space-y-1">
            <Label htmlFor="ip-block-comment">Comment</Label>
            <Input
              id="ip-block-comment"
              value={comment}
              placeholder="Why this rule exists"
              onChange={(e) => setComment(e.target.value)}
            />
          </div>
        </div>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={saving} />}>
            Cancel
          </DialogClose>
          <Button disabled={saving || !ip.trim()} onClick={() => void save()}>
            {saving ? 'Saving…' : 'Save rule'}
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
    toast.success(created ? `Added a rule for ${block.ip}.` : `Updated ${block.ip}.`)
  }

  return (
    <AdminLayout
      title="IP rules"
      permission="manage_blocks"
      sub={BLOCK_TABS}
      actions={
        <Button size="sm" onClick={() => setDialog({ editing: null })}>
          Add rule
        </Button>
      }
    >
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
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
                {b.expires_at ? `Expires ${formatDate(b.expires_at)}` : 'Never expires'}
              </div>
            </div>
            <Button size="xs" variant="outline" onClick={() => setDialog({ editing: b })}>
              Edit
            </Button>
            <ConfirmButton
              size="xs"
              title={`Remove the rule for ${b.ip}?`}
              description="The address range is treated like any other again."
              confirmLabel="Remove"
              onConfirm={async () => {
                await deleteIpBlock(token ?? '', b.id)
                feed.mutate((items) => items.filter((x) => x.id !== b.id))
                toast.success(`Removed the rule for ${b.ip}.`)
              }}
            >
              Remove
            </ConfirmButton>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No IP rules.</p>
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
