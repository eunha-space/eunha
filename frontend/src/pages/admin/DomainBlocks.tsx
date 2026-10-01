import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  createDomainBlock,
  deleteDomainBlock,
  existingDomainBlock,
  listDomainBlocks,
  updateDomainBlock,
  type DomainBlock,
  type DomainBlockSeverity,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
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
import { Textarea } from '@/components/ui/textarea.tsx'

export const FEDERATION_TABS = [
  { to: '/admin/domain_blocks', label: 'Domain blocks' },
  { to: '/admin/domain_allows', label: 'Domain allows' },
]

// Mastodon's labels for `DomainBlock#severity`, and what each does.
const SEVERITIES: Record<DomainBlockSeverity, string> = {
  silence: 'Limit',
  suspend: 'Suspend',
  noop: 'None',
}
const SEVERITY_HINTS: Record<DomainBlockSeverity, string> = {
  silence: "Hides the domain's posts from anyone not following its accounts.",
  suspend: "Removes everything stored from the domain and refuses its traffic.",
  noop: 'Only applies the options below.',
}

interface Draft {
  domain: string
  severity: DomainBlockSeverity
  reject_media: boolean
  reject_reports: boolean
  obfuscate: boolean
  private_comment: string
  public_comment: string
}

const blank: Draft = {
  domain: '',
  severity: 'silence',
  reject_media: false,
  reject_reports: false,
  obfuscate: false,
  private_comment: '',
  public_comment: '',
}

function draftOf(b: DomainBlock): Draft {
  return {
    domain: b.domain,
    severity: b.severity,
    reject_media: b.reject_media,
    reject_reports: b.reject_reports,
    obfuscate: b.obfuscate,
    private_comment: b.private_comment ?? '',
    public_comment: b.public_comment ?? '',
  }
}

/**
 * Create or edit a domain block. Creating one that an existing block already
 * covers is refused with that block attached — Mastodon's
 * `ExistingDomainBlockErrorSerializer` — and the form offers to edit it
 * instead, which is what upstream's admin form does too.
 */
function DomainBlockDialog({
  open,
  onOpenChange,
  editing,
  token,
  onSaved,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  editing: DomainBlock | null
  token: string
  onSaved: (block: DomainBlock, created: boolean) => void
}) {
  const [target, setTarget] = useState<DomainBlock | null>(editing)
  const [draft, setDraft] = useState<Draft>(editing ? draftOf(editing) : blank)
  const [conflict, setConflict] = useState<DomainBlock | null>(null)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (!open) return
    setTarget(editing)
    setDraft(editing ? draftOf(editing) : blank)
    setConflict(null)
  }, [open, editing])

  const set = <K extends keyof Draft>(key: K, value: Draft[K]) =>
    setDraft((d) => ({ ...d, [key]: value }))

  const save = async () => {
    setSaving(true)
    setConflict(null)
    const params = {
      severity: draft.severity,
      reject_media: draft.reject_media,
      reject_reports: draft.reject_reports,
      obfuscate: draft.obfuscate,
      private_comment: draft.private_comment.trim() || null,
      public_comment: draft.public_comment.trim() || null,
    }
    try {
      const block = target
        ? await updateDomainBlock(token, target.id, params)
        : await createDomainBlock(token, { domain: draft.domain.trim(), ...params })
      onSaved(block, !target)
      onOpenChange(false)
    } catch (e) {
      const existing = existingDomainBlock(e)
      if (existing) setConflict(existing)
      else toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{target ? `Edit block for ${target.domain}` : 'New domain block'}</DialogTitle>
          <DialogDescription>{SEVERITY_HINTS[draft.severity]}</DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          {!target && (
            <div className="space-y-1">
              <Label htmlFor="block-domain">Domain</Label>
              <Input
                id="block-domain"
                value={draft.domain}
                placeholder="example.com"
                onChange={(e) => set('domain', e.target.value)}
              />
            </div>
          )}
          <div className="space-y-1">
            <Label>Severity</Label>
            <ChoiceSelect
              label="Severity"
              value={draft.severity}
              items={SEVERITIES}
              onChange={(v) => set('severity', v)}
            />
          </div>
          <Label className="text-sm font-normal">
            <Checkbox
              checked={draft.reject_media}
              onCheckedChange={(v) => set('reject_media', v === true)}
            />
            Reject media files
          </Label>
          <Label className="text-sm font-normal">
            <Checkbox
              checked={draft.reject_reports}
              onCheckedChange={(v) => set('reject_reports', v === true)}
            />
            Reject reports
          </Label>
          <Label className="text-sm font-normal">
            <Checkbox
              checked={draft.obfuscate}
              onCheckedChange={(v) => set('obfuscate', v === true)}
            />
            Obfuscate the domain name when it is listed publicly
          </Label>
          <div className="space-y-1">
            <Label htmlFor="block-private">Private comment</Label>
            <Textarea
              id="block-private"
              rows={2}
              value={draft.private_comment}
              placeholder="For other moderators"
              onChange={(e) => set('private_comment', e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="block-public">Public comment</Label>
            <Textarea
              id="block-public"
              rows={2}
              value={draft.public_comment}
              placeholder="Shown with the domain on the about page, if listed"
              onChange={(e) => set('public_comment', e.target.value)}
            />
          </div>
          {conflict && (
            <div className="bg-muted space-y-2 rounded-lg p-3 text-sm">
              <p>
                {conflict.domain} is already blocked ({SEVERITIES[conflict.severity]}), and
                this block would not be stricter.
              </p>
              <Button
                size="sm"
                variant="outline"
                onClick={() => {
                  setTarget(conflict)
                  setDraft({ ...draft, domain: conflict.domain })
                  setConflict(null)
                }}
              >
                Edit the existing block instead
              </Button>
            </div>
          )}
        </div>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={saving} />}>
            Cancel
          </DialogClose>
          <Button
            disabled={saving || (!target && !draft.domain.trim())}
            onClick={() => void save()}
          >
            {saving ? 'Saving…' : target ? 'Save' : 'Block domain'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

export default function DomainBlocks() {
  const token = getToken()
  const feed = useInfinitePaginator<DomainBlock>(
    () => listDomainBlocks(token ?? ''),
    [token],
  )
  const [dialog, setDialog] = useState<{ editing: DomainBlock | null } | null>(null)

  const saved = (block: DomainBlock, created: boolean) => {
    feed.mutate((items) =>
      items.some((b) => b.id === block.id)
        ? items.map((b) => (b.id === block.id ? block : b))
        : [block, ...items],
    )
    toast.success(created ? `Blocked ${block.domain}.` : `Updated ${block.domain}.`)
  }

  return (
    <AdminLayout
      title="Domain blocks"
      permission="manage_federation"
      sub={FEDERATION_TABS}
      actions={
        <Button size="sm" onClick={() => setDialog({ editing: null })}>
          Add domain block
        </Button>
      }
    >
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className="space-y-2">
        {feed.items?.map((b) => (
          <div key={b.id} className="space-y-1.5 rounded-lg border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <span className="min-w-0 flex-1 truncate font-medium">{b.domain}</span>
              <Badge variant={b.severity === 'suspend' ? 'destructive' : 'secondary'}>
                {SEVERITIES[b.severity]}
              </Badge>
              {b.reject_media && <Badge variant="outline">No media</Badge>}
              {b.reject_reports && <Badge variant="outline">No reports</Badge>}
              {b.obfuscate && <Badge variant="outline">Obfuscated</Badge>}
            </div>
            {b.public_comment && <p className="text-sm">{b.public_comment}</p>}
            {b.private_comment && (
              <p className="text-muted-foreground text-sm italic">{b.private_comment}</p>
            )}
            <div className="flex items-center gap-2">
              <span className="text-muted-foreground flex-1 text-xs">
                {formatDate(b.created_at)}
              </span>
              <Button size="xs" variant="outline" onClick={() => setDialog({ editing: b })}>
                Edit
              </Button>
              <ConfirmButton
                size="xs"
                title={`Unblock ${b.domain}?`}
                description="The domain can federate with this server again. Anything already removed under a suspension does not come back."
                confirmLabel="Unblock"
                onConfirm={async () => {
                  await deleteDomainBlock(token ?? '', b.id)
                  feed.mutate((items) => items.filter((x) => x.id !== b.id))
                  toast.success(`Unblocked ${b.domain}.`)
                }}
              >
                Unblock
              </ConfirmButton>
            </div>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No domains are blocked.</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
      {token && (
        <DomainBlockDialog
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
