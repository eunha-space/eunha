import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import { createDomainBlock } from '../../admin-api.ts'
import {
  downloadExport,
  importDomainAllows,
  importDomainBlocks,
  listInstances,
  type AdminInstance,
  type ImportedDomainBlock,
  type InstanceFilters,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const MODERATION = { get all() { return t({ id: 'notifications.filter.all', defaultMessage: 'All' }) }, get limited() { return t({ id: 'about.domain_blocks.silenced.title', defaultMessage: 'Limited' }) } }
const AVAILABILITY = { get all() { return t({ id: 'notifications.filter.all', defaultMessage: 'All' }) }, get failing() { return t({ id: 'mastodon.admin.instances.delivery.failing', defaultMessage: 'Failing' }) }, get unavailable() { return t({ id: 'mastodon.admin.instances.delivery.unavailable', defaultMessage: 'Unavailable' }) } }

/** What a block does, as Mastodon's `policy_list` says it. */
export function policyText(block: {
  severity: string
  reject_media: boolean
  reject_reports: boolean
}): string {
  const parts: string[] = []
  if (block.severity === 'suspend') parts.push('Suspended')
  else if (block.severity === 'silence') parts.push('Limited')
  if (block.reject_media) parts.push('Reject media')
  if (block.reject_reports) parts.push('Reject reports')
  return parts.join(' · ') || t({ id: 'eunha.admin.instances.no_restrictions', defaultMessage: 'No restrictions' })
}

/**
 * Mastodon's domain block import: the rows the file would block, to pick from
 * before any is created, as its confirmation form does.
 */
function BlockImport({ onDone }: { onDone: () => void }) {
  const token = getToken()
  const [candidates, setCandidates] = useState<ImportedDomainBlock[] | null>(null)
  const [warnings, setWarnings] = useState<string[]>([])
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [busy, setBusy] = useState(false)

  const read = async (file: File | undefined) => {
    if (!file) return
    try {
      const result = await importDomainBlocks(token ?? '', file)
      setCandidates(result.domain_blocks)
      setWarnings(result.warning_domains)
      setPicked(new Set(result.domain_blocks.map((b) => b.domain)))
      for (const error of result.errors) toast.error(error)
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  const save = async () => {
    setBusy(true)
    let created = 0
    for (const block of candidates ?? []) {
      if (!picked.has(block.domain)) continue
      try {
        await createDomainBlock(token ?? '', { ...block })
        created++
      } catch (e) {
        toast.error(`${block.domain}: ${errorMessage(e)}`)
      }
    }
    setBusy(false)
    toast.success(t({ id: 'eunha.admin.instances.domain_blocks_created', defaultMessage: '{created} domain blocks created.' }, { created }))
    setCandidates(null)
    onDone()
  }

  return (
    <div className="space-y-2 rounded-lg border p-3">
      <Label htmlFor="import-blocks">{t({ id: 'mastodon.admin.export_domain_blocks.import.title', defaultMessage: 'Import domain blocks' })}</Label>
      <Input
        id="import-blocks"
        type="file"
        accept=".csv,text/csv"
        onChange={(e) => void read(e.target.files?.[0])}
      />
      {candidates && candidates.length === 0 && (
        <p className="text-muted-foreground text-sm">
          {t({ id: 'eunha.admin.instances.nothing_to_import_every_domain_in', defaultMessage: 'Nothing to import: every domain in the file is already blocked.' })}
        </p>
      )}
      {candidates && candidates.length > 0 && (
        <div className="space-y-2">
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.admin.instances.pick_the_blocks_to_create_domains', defaultMessage: 'Pick the blocks to create. Domains marked as followed have local accounts that follow or are followed by their accounts, which a block would sever.' })}
          </p>
          {candidates.map((b) => (
            <Label key={b.domain} className="text-sm font-normal">
              <Checkbox
                checked={picked.has(b.domain)}
                onCheckedChange={(on) =>
                  setPicked((set) => {
                    const next = new Set(set)
                    if (on) next.add(b.domain)
                    else next.delete(b.domain)
                    return next
                  })
                }
              />
              <span className="font-medium">{b.domain}</span>
              <span className="text-muted-foreground">{policyText(b)}</span>
              {warnings.includes(b.domain) && <Badge variant="destructive">{t({ id: 'eunha.admin.instances.followed', defaultMessage: 'Followed' })}</Badge>}
            </Label>
          ))}
          <Button size="sm" disabled={busy || picked.size === 0} onClick={() => void save()}>
            {t({ id: 'eunha.admin.instances.block_domains', defaultMessage: 'Block {count, plural, one {# domain} other {# domains}}' }, { count: picked.size })}
          </Button>
        </div>
      )}
    </div>
  )
}

/**
 * Federation: Mastodon's `Admin::InstancesController#index`, every server this
 * one knows, with exports and imports of the block and allow lists.
 */
export default function Instances() {
  const token = getToken()
  const [moderation, setModeration] = useState<keyof typeof MODERATION>('all')
  const [availability, setAvailability] = useState<keyof typeof AVAILABILITY>('all')
  const [byDomain, setByDomain] = useState('')
  const [page, setPage] = useState(1)
  const [items, setItems] = useState<AdminInstance[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const filters: InstanceFilters = {
    limited: moderation === 'limited',
    availability: availability === 'all' ? undefined : availability,
    by_domain: byDomain.trim() || undefined,
    page,
  }
  const load = () => {
    if (!token) return
    listInstances(token, filters)
      .then((list) => {
        setItems(list)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token, moderation, availability, byDomain, page])

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.instances.title', defaultMessage: 'Federation' })} permission="manage_federation">
      <div className="mb-3 flex flex-wrap items-end gap-2">
        <div className="space-y-1">
          <Label>{t({ id: 'navigation_bar.moderation', defaultMessage: 'Moderation' })}</Label>
          <ChoiceSelect
            label={t({ id: 'navigation_bar.moderation', defaultMessage: 'Moderation' })}
            value={moderation}
            items={MODERATION}
            onChange={(v) => {
              setModeration(v)
              setPage(1)
            }}
            className="w-36"
          />
        </div>
        <div className="space-y-1">
          <Label>{t({ id: 'mastodon.admin.instances.availability.title', defaultMessage: 'Availability' })}</Label>
          <ChoiceSelect
            label={t({ id: 'mastodon.admin.instances.availability.title', defaultMessage: 'Availability' })}
            value={availability}
            items={AVAILABILITY}
            onChange={(v) => {
              setAvailability(v)
              setPage(1)
            }}
            className="w-36"
          />
        </div>
        <div className="min-w-40 flex-1 space-y-1">
          <Label htmlFor="by-domain">{t({ id: 'mastodon.admin.instances.by_domain', defaultMessage: 'Domain' })}</Label>
          <Input
            id="by-domain"
            value={byDomain}
            placeholder="example.com"
            onChange={(e) => {
              setByDomain(e.target.value)
              setPage(1)
            }}
          />
        </div>
      </div>
      <AdminError error={error} />
      {items === null && !error && <RowsSkeleton count={6} />}
      {items?.length === 0 && <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.instances.no_servers_found', defaultMessage: 'No servers found.' })}</p>}
      <div className="divide-y rounded-lg border">
        {items?.map((i) => (
          <Link
            key={i.domain}
            to={`/admin/instances/${encodeURIComponent(i.domain)}`}
            className="hover:bg-muted/50 flex flex-wrap items-center gap-2 px-3 py-2 text-sm no-underline"
          >
            <span className="min-w-0 flex-1 truncate font-medium">{i.domain}</span>
            {i.domain_block && <Badge variant="outline">{policyText(i.domain_block)}</Badge>}
            {i.domain_allow && <Badge variant="outline">{t({ id: 'mastodon.admin.accounts.follow_recommendations.allowed', defaultMessage: 'Allowed' })}</Badge>}
            {i.unavailable && <Badge variant="destructive">{t({ id: 'mastodon.admin.instances.delivery.unavailable', defaultMessage: 'Unavailable' })}</Badge>}
            {!i.unavailable && i.failure_days !== null && (
              <Badge variant="outline">
                {t({ id: 'eunha.admin.instances.failing_for', defaultMessage: 'Failing for {count, plural, one {# day} other {# days}}' }, { count: i.failure_days })}
              </Badge>
            )}
            <span className="text-muted-foreground text-xs tabular-nums">
              {i.accounts_count} {i.accounts_count === 1 ? 'account' : 'accounts'}
            </span>
          </Link>
        ))}
      </div>
      <div className="mt-2 flex gap-2">
        <Button size="sm" variant="outline" disabled={page === 1} onClick={() => setPage(page - 1)}>
          {t({ id: 'mastodon.pagination.newer', defaultMessage: 'Newer' })}
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={(items?.length ?? 0) < 40}
          onClick={() => setPage(page + 1)}
        >
          {t({ id: 'mastodon.pagination.older', defaultMessage: 'Older' })}
        </Button>
      </div>

      <h2 className="mt-6 mb-2 text-sm font-semibold">{t({ id: 'eunha.admin.instances.block_and_allow_lists', defaultMessage: 'Block and allow lists' })}</h2>
      <div className="mb-3 flex flex-wrap gap-2">
        <Button
          size="sm"
          variant="outline"
          onClick={() =>
            void downloadExport(token ?? '', 'domain_blocks').catch((e) =>
              toast.error(errorMessage(e)),
            )
          }
        >
          {t({ id: 'eunha.admin.instances.export_domain_blocks', defaultMessage: 'Export domain blocks' })}
        </Button>
        <Button
          size="sm"
          variant="outline"
          onClick={() =>
            void downloadExport(token ?? '', 'domain_allows').catch((e) =>
              toast.error(errorMessage(e)),
            )
          }
        >
          {t({ id: 'eunha.admin.instances.export_domain_allows', defaultMessage: 'Export domain allows' })}
        </Button>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <BlockImport onDone={load} />
        <div className="space-y-2 rounded-lg border p-3">
          <Label htmlFor="import-allows">{t({ id: 'mastodon.admin.export_domain_allows.new.title', defaultMessage: 'Import domain allows' })}</Label>
          <Input
            id="import-allows"
            type="file"
            accept=".csv,text/csv"
            onChange={async (e) => {
              const file = e.target.files?.[0]
              if (!file) return
              try {
                const created = await importDomainAllows(token ?? '', file)
                toast.success(t({ id: 'eunha.admin.instances.domains_allowed', defaultMessage: '{length} domains allowed.' }, { length: created.length }))
                load()
              } catch (err) {
                toast.error(errorMessage(err))
              }
            }}
          />
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.admin.instances.every_domain_in_the_file_not', defaultMessage: 'Every domain in the file not yet allowed is allowed at once.' })}
          </p>
        </div>
      </div>
    </AdminLayout>
  )
}
