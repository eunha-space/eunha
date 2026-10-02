import { useCallback, useEffect, useState } from 'react'
import { Download, Upload } from 'lucide-react'
import { toast } from 'sonner'

import {
  ApiError,
  cancelImport,
  confirmImport,
  downloadFile,
  getBackupDownloadUrl,
  getExportSummary,
  getRecentImports,
  requestBackup,
  uploadImport,
  type BulkImport,
  type ExportSummary,
  type ImportType,
} from '../eunha-api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group.tsx'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table.tsx'

// Mastodon's `imports.types.*`.
const TYPES: Record<ImportType, string> = {
  following: 'Following list',
  bookmarks: 'Bookmarks',
  lists: 'Lists',
  muting: 'Muting list',
  blocking: 'Blocking list',
  domain_blocking: 'Domain blocking list',
  custom_filters: 'Filters',
}

// Mastodon's `imports.states.*`.
const STATES: Record<BulkImport['state'], string> = {
  unconfirmed: 'Unconfirmed',
  scheduled: 'Scheduled',
  in_progress: 'In progress',
  finished: 'Finished',
}

// What each export row downloads, in the order Mastodon's page lists them.
const EXPORTS: { label: string; count: keyof ExportSummary; file?: string }[] = [
  { label: 'Posts', count: 'statuses' },
  { label: 'Follows', count: 'follows', file: 'follows.csv' },
  { label: 'Lists', count: 'lists', file: 'lists.csv' },
  { label: 'Followers', count: 'followers' },
  { label: 'You mute', count: 'mutes', file: 'mutes.csv' },
  { label: 'You block', count: 'blocks', file: 'blocks.csv' },
  { label: 'Domain blocks', count: 'domain_blocks', file: 'domain_blocks.csv' },
  { label: 'Bookmarks', count: 'bookmarks', file: 'bookmarks.csv' },
  { label: 'Filters', count: 'custom_filters', file: 'custom_filters.json' },
]

/** `number_to_human_size`. */
function humanSize(bytes: number): string {
  const units = ['Bytes', 'KB', 'MB', 'GB', 'TB']
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return unit === 0 ? `${value} Bytes` : `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`
}

/** Mastodon's `imports.preambles` / `overwrite_preambles`, without the markup. */
function preamble(pending: BulkImport): string {
  const n = pending.total_items
  const accounts = `${n} ${n === 1 ? 'account' : 'accounts'}`
  const from = `from ${pending.original_filename}`
  if (pending.overwrite) {
    switch (pending.type) {
      case 'following':
        return `You are about to follow up to ${accounts} ${from} and stop following anyone else.`
      case 'blocking':
        return `You are about to replace your block list with up to ${accounts} ${from}.`
      case 'muting':
        return `You are about to replace your list of muted accounts with up to ${accounts} ${from}.`
      case 'domain_blocking':
        return `You are about to replace your domain block list with up to ${n} ${n === 1 ? 'domain' : 'domains'} ${from}.`
      case 'bookmarks':
        return `You are about to replace your bookmarks with up to ${n} ${n === 1 ? 'post' : 'posts'} ${from}.`
      case 'lists':
        return `You are about to replace your lists with contents of ${pending.original_filename}. Up to ${accounts} will be added to new lists.`
      case 'custom_filters':
        return `You are about to replace your filters with contents of ${pending.original_filename}. Up to ${n} ${n === 1 ? 'filter' : 'filters'} will be added to new filters.`
    }
  }
  switch (pending.type) {
    case 'following':
      return `You are about to follow up to ${accounts} ${from}.`
    case 'blocking':
      return `You are about to block up to ${accounts} ${from}.`
    case 'muting':
      return `You are about to mute up to ${accounts} ${from}.`
    case 'domain_blocking':
      return `You are about to block up to ${n} ${n === 1 ? 'domain' : 'domains'} ${from}.`
    case 'bookmarks':
      return `You are about to add up to ${n} ${n === 1 ? 'post' : 'posts'} ${from} to your bookmarks.`
    case 'lists':
      return `You are about to add up to ${accounts} ${from} to your lists. New lists will be created if there is no list to add to.`
    case 'custom_filters':
      return `You are about to add up to ${n} ${n === 1 ? 'filter' : 'filters'} ${from} to your filters.`
  }
}

function Exports({ token }: { token: string }) {
  const [summary, setSummary] = useState<ExportSummary | null>(null)
  const [requesting, setRequesting] = useState(false)

  const load = useCallback(() => {
    getExportSummary(token)
      .then(setSummary)
      .catch(() => toast.error('Could not load your export'))
  }, [token])
  useEffect(load, [load])

  const download = async (file: string) => {
    try {
      await downloadFile(token, `/api/eunha/v1/exports/${file}`)
    } catch {
      toast.error('Could not download the file')
    }
  }

  const request = async () => {
    setRequesting(true)
    try {
      await requestBackup(token)
      load()
    } catch {
      toast.error('Could not request your archive')
    } finally {
      setRequesting(false)
    }
  }

  const openBackup = async (id: string) => {
    try {
      window.location.assign(await getBackupDownloadUrl(token, id))
    } catch {
      toast.error('Could not download your archive')
    }
  }

  if (!summary) return null

  return (
    <section className="space-y-4 rounded-lg border p-4">
      <h2 className="font-semibold">Data export</h2>
      <Table>
        <TableBody>
          <TableRow>
            <TableHead>Media storage</TableHead>
            <TableCell>{humanSize(summary.storage)}</TableCell>
            <TableCell />
          </TableRow>
          {EXPORTS.map(({ label, count, file }) => (
            <TableRow key={label}>
              <TableHead>{label}</TableHead>
              <TableCell>{Number(summary[count]).toLocaleString()}</TableCell>
              <TableCell className="text-right">
                {file && (
                  <Button variant="ghost" size="sm" onClick={() => download(file)}>
                    <Download /> {file.endsWith('.json') ? 'JSON' : 'CSV'}
                  </Button>
                )}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>

      <p className="text-muted-foreground text-sm">
        You can request an archive of your <strong>posts and uploaded media</strong>. The
        exported data will be in the ActivityPub format, readable by any compliant
        software. You can request an archive every 7 days.
      </p>
      {summary.can_request_backup && (
        <Button size="sm" disabled={requesting} onClick={request}>
          Request your archive
        </Button>
      )}
      {summary.backups.length > 0 && (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Date</TableHead>
              <TableHead>Size</TableHead>
              <TableHead />
            </TableRow>
          </TableHeader>
          <TableBody>
            {summary.backups.map((backup) => (
              <TableRow key={backup.id}>
                <TableCell>{new Date(backup.created_at).toLocaleString()}</TableCell>
                {backup.processed ? (
                  <>
                    <TableCell>{humanSize(backup.dump_file_size ?? 0)}</TableCell>
                    <TableCell className="text-right">
                      <Button variant="ghost" size="sm" onClick={() => openBackup(backup.id)}>
                        <Download /> Download your archive
                      </Button>
                    </TableCell>
                  </>
                ) : (
                  <TableCell colSpan={2}>Compiling your archive...</TableCell>
                )}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </section>
  )
}

function Imports({ token }: { token: string }) {
  const [type, setType] = useState<ImportType>('following')
  const [mode, setMode] = useState<'merge' | 'overwrite'>('merge')
  const [file, setFile] = useState<File | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [pending, setPending] = useState<BulkImport | null>(null)
  const [recent, setRecent] = useState<BulkImport[]>([])

  const loadRecent = useCallback(() => {
    getRecentImports(token)
      .then(setRecent)
      .catch(() => {})
  }, [token])
  useEffect(loadRecent, [loadRecent])

  // An import in progress moves on by itself; look again now and then.
  useEffect(() => {
    if (!recent.some((r) => r.state === 'scheduled' || r.state === 'in_progress')) return
    const timer = window.setTimeout(loadRecent, 5000)
    return () => window.clearTimeout(timer)
  }, [recent, loadRecent])

  const upload = async () => {
    if (!file) return
    setBusy(true)
    setError(null)
    try {
      setPending(await uploadImport(token, type, mode, file))
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not upload the file')
    } finally {
      setBusy(false)
    }
  }

  const confirm = async () => {
    if (!pending) return
    setBusy(true)
    try {
      await confirmImport(token, pending.id)
      toast.success('Your data was successfully uploaded and will be processed in due time')
      setPending(null)
      setFile(null)
      loadRecent()
    } catch {
      toast.error('Could not start the import')
    } finally {
      setBusy(false)
    }
  }

  const cancel = async () => {
    if (!pending) return
    await cancelImport(token, pending.id).catch(() => {})
    setPending(null)
    loadRecent()
  }

  return (
    <section className="space-y-4 rounded-lg border p-4">
      <h2 className="font-semibold">Import</h2>
      {pending ? (
        <div className="space-y-3">
          <p className="text-sm">{preamble(pending)}</p>
          {pending.likely_mismatched && (
            <p className="text-destructive text-sm">
              It appears you may have selected the wrong type for this import, please
              double-check.
            </p>
          )}
          {pending.missing_status && (
            <p className="text-muted-foreground text-sm">
              Some of your filters hide specific posts that are not known by this server.
              These posts will not be automatically filtered if they are discovered later
              by the server.
            </p>
          )}
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={confirm}>
              Confirm
            </Button>
            <Button size="sm" variant="secondary" disabled={busy} onClick={cancel}>
              Cancel
            </Button>
          </div>
        </div>
      ) : (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault()
            void upload()
          }}
        >
          <p className="text-muted-foreground text-sm">
            You can import data that you have exported from another server, such as a list
            of the people you are following or blocking.
          </p>
          <div className="space-y-1">
            <Label>Import type</Label>
            <Select
              items={TYPES}
              value={type}
              onValueChange={(v) => setType((v as ImportType | null) ?? 'following')}
            >
              <SelectTrigger className="w-full" aria-label="Import type">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  {Object.entries(TYPES).map(([value, label]) => (
                    <SelectItem key={value} value={value}>
                      {label}
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="import-file">Data</Label>
            <Input
              id="import-file"
              type="file"
              accept={type === 'custom_filters' ? '.json,application/json' : '.csv,text/csv'}
              onChange={(e) => setFile(e.target.files?.[0] ?? null)}
            />
            {error && <p className="text-destructive text-xs">{error}</p>}
          </div>
          <RadioGroup
            value={mode}
            onValueChange={(v) => setMode(v === 'overwrite' ? 'overwrite' : 'merge')}
          >
            <Label className="text-sm font-normal">
              <RadioGroupItem value="merge" /> Merge — keep existing records and add new ones
            </Label>
            <Label className="text-sm font-normal">
              <RadioGroupItem value="overwrite" /> Overwrite — replace current records with
              the new ones
            </Label>
          </RadioGroup>
          <Button type="submit" size="sm" disabled={!file || busy}>
            <Upload /> Upload
          </Button>
        </form>
      )}

      {recent.length > 0 && (
        <>
          <h3 className="text-sm font-semibold">Recent imports</h3>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Type</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Started at</TableHead>
                <TableHead>Imported</TableHead>
                <TableHead>Failures</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {recent.map((r) => (
                <TableRow key={r.id}>
                  <TableCell>{TYPES[r.type]}</TableCell>
                  <TableCell>
                    <Badge variant="secondary">{STATES[r.state]}</Badge>
                  </TableCell>
                  <TableCell>{new Date(r.created_at).toLocaleString()}</TableCell>
                  <TableCell>
                    {r.imported_items} / {r.total_items}
                  </TableCell>
                  <TableCell>
                    {r.state === 'finished' && r.failure_count > 0 ? (
                      <Button
                        variant="link"
                        size="sm"
                        onClick={() =>
                          downloadFile(token, `/api/eunha/v1/imports/${r.id}/failures`).catch(
                            () => toast.error('Could not download the failures'),
                          )
                        }
                      >
                        {r.failure_count}
                      </Button>
                    ) : (
                      r.failure_count
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </>
      )}
    </section>
  )
}

/** Mastodon's "Import and export" settings: `/settings/export` and `/settings/imports`. */
export default function ImportExport() {
  const token = getToken()
  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-4 text-lg font-bold">Import and export</h1>
      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">Sign in to manage your data.</p>
          <Button size="sm" onClick={() => beginLogin()}>
            Sign in
          </Button>
        </div>
      ) : (
        <div className="space-y-4">
          <Exports token={token} />
          <Imports token={token} />
        </div>
      )}
    </div>
  )
}
