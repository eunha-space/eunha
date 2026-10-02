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

const MODERATION = { all: 'All', limited: 'Limited' }
const AVAILABILITY = { all: 'All', failing: 'Failing', unavailable: 'Unavailable' }

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
  return parts.join(' · ') || 'No restrictions'
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
    toast.success(`${created} domain blocks created.`)
    setCandidates(null)
    onDone()
  }

  return (
    <div className="space-y-2 rounded-lg border p-3">
      <Label htmlFor="import-blocks">Import domain blocks</Label>
      <Input
        id="import-blocks"
        type="file"
        accept=".csv,text/csv"
        onChange={(e) => void read(e.target.files?.[0])}
      />
      {candidates && candidates.length === 0 && (
        <p className="text-muted-foreground text-sm">
          Nothing to import: every domain in the file is already blocked.
        </p>
      )}
      {candidates && candidates.length > 0 && (
        <div className="space-y-2">
          <p className="text-muted-foreground text-xs">
            Pick the blocks to create. Domains marked as followed have local accounts that
            follow or are followed by their accounts, which a block would sever.
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
              {warnings.includes(b.domain) && <Badge variant="destructive">Followed</Badge>}
            </Label>
          ))}
          <Button size="sm" disabled={busy || picked.size === 0} onClick={() => void save()}>
            Block {picked.size} domains
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
    <AdminLayout title="Federation" permission="manage_federation">
      <div className="mb-3 flex flex-wrap items-end gap-2">
        <div className="space-y-1">
          <Label>Moderation</Label>
          <ChoiceSelect
            label="Moderation"
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
          <Label>Availability</Label>
          <ChoiceSelect
            label="Availability"
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
          <Label htmlFor="by-domain">Domain</Label>
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
      {items === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {items?.length === 0 && <p className="text-muted-foreground text-sm">No servers found.</p>}
      <div className="divide-y rounded-lg border">
        {items?.map((i) => (
          <Link
            key={i.domain}
            to={`/admin/instances/${encodeURIComponent(i.domain)}`}
            className="hover:bg-muted/50 flex flex-wrap items-center gap-2 px-3 py-2 text-sm no-underline"
          >
            <span className="min-w-0 flex-1 truncate font-medium">{i.domain}</span>
            {i.domain_block && <Badge variant="outline">{policyText(i.domain_block)}</Badge>}
            {i.domain_allow && <Badge variant="outline">Allowed</Badge>}
            {i.unavailable && <Badge variant="destructive">Unavailable</Badge>}
            {!i.unavailable && i.failure_days !== null && (
              <Badge variant="outline">
                Failing for {i.failure_days} {i.failure_days === 1 ? 'day' : 'days'}
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
          Newer
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={(items?.length ?? 0) < 40}
          onClick={() => setPage(page + 1)}
        >
          Older
        </Button>
      </div>

      <h2 className="mt-6 mb-2 text-sm font-semibold">Block and allow lists</h2>
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
          Export domain blocks
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
          Export domain allows
        </Button>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <BlockImport onDone={load} />
        <div className="space-y-2 rounded-lg border p-3">
          <Label htmlFor="import-allows">Import domain allows</Label>
          <Input
            id="import-allows"
            type="file"
            accept=".csv,text/csv"
            onChange={async (e) => {
              const file = e.target.files?.[0]
              if (!file) return
              try {
                const created = await importDomainAllows(token ?? '', file)
                toast.success(`${created.length} domains allowed.`)
                load()
              } catch (err) {
                toast.error(errorMessage(err))
              }
            }}
          />
          <p className="text-muted-foreground text-xs">
            Every domain in the file not yet allowed is allowed at once.
          </p>
        </div>
      </div>
    </AdminLayout>
  )
}
