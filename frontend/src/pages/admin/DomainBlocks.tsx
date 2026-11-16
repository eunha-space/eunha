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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

export const FEDERATION_TABS = [
  { to: '/admin/domain_blocks', get label() { return t({ id: 'mastodon.exports.domain_blocks', defaultMessage: 'Domain blocks' }) } },
  { to: '/admin/domain_allows', get label() { return t({ id: 'eunha.admin.domain_blocks.domain_allows', defaultMessage: 'Domain allows' }) } },
]

// Mastodon's labels for `DomainBlock#severity`, and what each does.
const SEVERITIES: Record<DomainBlockSeverity, string> = {
  get silence() { return t({ id: 'mastodon.admin.domain_blocks.new.severity.silence', defaultMessage: 'Limit' }) },
  get suspend() { return t({ id: 'mastodon.admin.domain_blocks.confirm_suspension.confirm', defaultMessage: 'Suspend' }) },
  get noop() { return t({ id: 'mastodon.admin.domain_blocks.new.severity.noop', defaultMessage: 'None' }) },
}
const SEVERITY_HINTS: Record<DomainBlockSeverity, string> = {
  get silence() { return t({ id: 'eunha.admin.domain_blocks.hides_the_domain_s_posts_from', defaultMessage: 'Hides the domain\'\'s posts from anyone not following its accounts.' }) },
  get suspend() { return t({ id: 'eunha.admin.domain_blocks.removes_everything_stored_from_the_domain', defaultMessage: 'Removes everything stored from the domain and refuses its traffic.' }) },
  get noop() { return t({ id: 'eunha.admin.domain_blocks.only_applies_the_options_below', defaultMessage: 'Only applies the options below.' }) },
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
          <DialogTitle>{target ? t({ id: 'eunha.admin.domain_blocks.edit_block_for', defaultMessage: 'Edit block for {domain}' }, { domain: target.domain }) : t({ id: 'mastodon.admin.domain_blocks.new.title', defaultMessage: 'New domain block' })}</DialogTitle>
          <DialogDescription>{SEVERITY_HINTS[draft.severity]}</DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          {!target && (
            <div className="space-y-1">
              <Label htmlFor="block-domain">{t({ id: 'mastodon.admin.domain_blocks.domain', defaultMessage: 'Domain' })}</Label>
              <Input
                id="block-domain"
                value={draft.domain}
                placeholder="example.com"
                onChange={(e) => set('domain', e.target.value)}
              />
            </div>
          )}
          <div className="space-y-1">
            <Label>{t({ id: 'mastodon.simple_form.labels.defaults.severity', defaultMessage: 'Severity' })}</Label>
            <ChoiceSelect
              label={t({ id: 'mastodon.simple_form.labels.defaults.severity', defaultMessage: 'Severity' })}
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
            {t({ id: 'mastodon.admin.domain_blocks.reject_media', defaultMessage: 'Reject media files' })}
          </Label>
          <Label className="text-sm font-normal">
            <Checkbox
              checked={draft.reject_reports}
              onCheckedChange={(v) => set('reject_reports', v === true)}
            />
            {t({ id: 'mastodon.admin.domain_blocks.reject_reports', defaultMessage: 'Reject reports' })}
          </Label>
          <Label className="text-sm font-normal">
            <Checkbox
              checked={draft.obfuscate}
              onCheckedChange={(v) => set('obfuscate', v === true)}
            />
            {t({ id: 'eunha.admin.domain_blocks.obfuscate_the_domain_name_when_it', defaultMessage: 'Obfuscate the domain name when it is listed publicly' })}
          </Label>
          <div className="space-y-1">
            <Label htmlFor="block-private">{t({ id: 'mastodon.admin.domain_blocks.private_comment', defaultMessage: 'Private comment' })}</Label>
            <Textarea
              id="block-private"
              rows={2}
              value={draft.private_comment}
              placeholder={t({ id: 'eunha.admin.domain_blocks.for_other_moderators', defaultMessage: 'For other moderators' })}
              onChange={(e) => set('private_comment', e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="block-public">{t({ id: 'mastodon.admin.domain_blocks.public_comment', defaultMessage: 'Public comment' })}</Label>
            <Textarea
              id="block-public"
              rows={2}
              value={draft.public_comment}
              placeholder={t({ id: 'eunha.admin.domain_blocks.shown_with_the_domain_on_the', defaultMessage: 'Shown with the domain on the about page, if listed' })}
              onChange={(e) => set('public_comment', e.target.value)}
            />
          </div>
          {conflict && (
            <div className="bg-muted space-y-2 rounded-lg p-3 text-sm">
              <p>
                {t(
                  { id: 'eunha.admin.domain_blocks.already_blocked', defaultMessage: '{domain} is already blocked ({severity}), and this block would not be stricter.' },
                  { domain: conflict.domain, severity: SEVERITIES[conflict.severity] },
                )}
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
                {t({ id: 'eunha.admin.domain_blocks.edit_the_existing_block_instead', defaultMessage: 'Edit the existing block instead' })}
              </Button>
            </div>
          )}
        </div>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={saving} />}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
          </DialogClose>
          <Button
            disabled={saving || (!target && !draft.domain.trim())}
            onClick={() => void save()}
          >
            {saving ? t({ id: 'eunha.common.saving', defaultMessage: 'Saving…' }) : target ? t({ id: 'lists.save', defaultMessage: 'Save' }) : t({ id: 'eunha.admin.domain_blocks.block_domain', defaultMessage: 'Block domain' })}
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
    toast.success(created ? t({ id: 'eunha.admin.domain_blocks.blocked', defaultMessage: 'Blocked {domain}.' }, { domain: block.domain }) : t({ id: 'eunha.admin.domain_blocks.updated', defaultMessage: 'Updated {domain}.' }, { domain: block.domain }))
  }

  return (
    <AdminLayout
      title={t({ id: 'mastodon.exports.domain_blocks', defaultMessage: 'Domain blocks' })}
      permission="manage_federation"
      sub={FEDERATION_TABS}
      actions={
        <Button size="sm" onClick={() => setDialog({ editing: null })}>
          {t({ id: 'eunha.admin.domain_blocks.add_domain_block', defaultMessage: 'Add domain block' })}
        </Button>
      }
    >
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <RowsSkeleton action />
      )}
      <div className="space-y-2">
        {feed.items?.map((b) => (
          <div key={b.id} className="space-y-1.5 rounded-lg border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <span className="min-w-0 flex-1 truncate font-medium">{b.domain}</span>
              <Badge variant={b.severity === 'suspend' ? 'destructive' : 'secondary'}>
                {SEVERITIES[b.severity]}
              </Badge>
              {b.reject_media && <Badge variant="outline">{t({ id: 'eunha.admin.domain_blocks.no_media', defaultMessage: 'No media' })}</Badge>}
              {b.reject_reports && <Badge variant="outline">{t({ id: 'eunha.admin.domain_blocks.no_reports', defaultMessage: 'No reports' })}</Badge>}
              {b.obfuscate && <Badge variant="outline">{t({ id: 'eunha.admin.domain_blocks.obfuscated', defaultMessage: 'Obfuscated' })}</Badge>}
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
                {t({ id: 'status.edit', defaultMessage: 'Edit' })}
              </Button>
              <ConfirmButton
                size="xs"
                title={t({ id: 'eunha.admin.domain_blocks.unblock', defaultMessage: 'Unblock {domain}?' }, { domain: b.domain })}
                description={t({ id: 'eunha.admin.domain_blocks.the_domain_can_federate_with_this', defaultMessage: 'The domain can federate with this server again. Anything already removed under a suspension does not come back.' })}
                confirmLabel={t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
                onConfirm={async () => {
                  await deleteDomainBlock(token ?? '', b.id)
                  feed.mutate((items) => items.filter((x) => x.id !== b.id))
                  toast.success(t({ id: 'eunha.admin.domain_blocks.unblocked', defaultMessage: 'Unblocked {domain}.' }, { domain: b.domain }))
                }}
              >
                {t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
              </ConfirmButton>
            </div>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.domain_blocks.no_domains_are_blocked', defaultMessage: 'No domains are blocked.' })}</p>
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
