import { useCallback, useEffect, useState, type ReactNode } from 'react'
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
import { SettingsLayout } from '@/components/settings-layout.tsx'
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
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

// Mastodon's `imports.types.*`.
const TYPES: Record<ImportType, string> = {
  get following() { return t({ id: 'mastodon.imports.types.following', defaultMessage: 'Following list' }) },
  get bookmarks() { return t({ id: 'column.bookmarks', defaultMessage: 'Bookmarks' }) },
  get lists() { return t({ id: 'column.lists', defaultMessage: 'Lists' }) },
  get muting() { return t({ id: 'mastodon.imports.types.muting', defaultMessage: 'Muting list' }) },
  get blocking() { return t({ id: 'mastodon.imports.types.blocking', defaultMessage: 'Blocking list' }) },
  get domain_blocking() { return t({ id: 'mastodon.imports.types.domain_blocking', defaultMessage: 'Domain blocking list' }) },
  get custom_filters() { return t({ id: 'mastodon.exports.custom_filters', defaultMessage: 'Filters' }) },
}

// Mastodon's `imports.states.*`.
const STATES: Record<BulkImport['state'], string> = {
  get unconfirmed() { return t({ id: 'mastodon.imports.states.unconfirmed', defaultMessage: 'Unconfirmed' }) },
  get scheduled() { return t({ id: 'mastodon.imports.states.scheduled', defaultMessage: 'Scheduled' }) },
  get in_progress() { return t({ id: 'mastodon.imports.states.in_progress', defaultMessage: 'In progress' }) },
  get finished() { return t({ id: 'mastodon.imports.states.finished', defaultMessage: 'Finished' }) },
}

// What each export row downloads, in the order Mastodon's page lists them.
const EXPORTS: { label: string; count: keyof ExportSummary; file?: string }[] = [
  { get label() { return t({ id: 'account.posts', defaultMessage: 'Posts' }) }, count: 'statuses' },
  { get label() { return t({ id: 'notifications.filter.follows', defaultMessage: 'Follows' }) }, count: 'follows', file: 'follows.csv' },
  { get label() { return t({ id: 'column.lists', defaultMessage: 'Lists' }) }, count: 'lists', file: 'lists.csv' },
  { get label() { return t({ id: 'account.followers', defaultMessage: 'Followers' }) }, count: 'followers' },
  { get label() { return t({ id: 'mastodon.exports.mutes', defaultMessage: 'You mute' }) }, count: 'mutes', file: 'mutes.csv' },
  { get label() { return t({ id: 'mastodon.exports.blocks', defaultMessage: 'You block' }) }, count: 'blocks', file: 'blocks.csv' },
  { get label() { return t({ id: 'mastodon.exports.domain_blocks', defaultMessage: 'Domain blocks' }) }, count: 'domain_blocks', file: 'domain_blocks.csv' },
  { get label() { return t({ id: 'column.bookmarks', defaultMessage: 'Bookmarks' }) }, count: 'bookmarks', file: 'bookmarks.csv' },
  { get label() { return t({ id: 'mastodon.exports.custom_filters', defaultMessage: 'Filters' }) }, count: 'custom_filters', file: 'custom_filters.json' },
]

/** `number_to_human_size`. */
function humanSize(bytes: number): string {
  const units = ['Bytes', 'KB', 'MB', 'GB', 'TB'] // i18n-ignore: Bytes is translated below
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return unit === 0 ? t({ id: 'eunha.import_export.bytes', defaultMessage: '{value} Bytes' }, { value }) : `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`
}

/** Mastodon's `imports.preambles` / `overwrite_preambles`. */
function preamble(pending: BulkImport): ReactNode {
  const values = {
    count: pending.total_items,
    filename: pending.original_filename,
    strong: (chunks: ReactNode[]) => <strong>{chunks}</strong>,
  }
  if (pending.overwrite) {
    switch (pending.type) {
      case 'following':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.following_html'
            defaultMessage="{count, plural, one {You are about to <strong>follow</strong> up to <strong>{count} account</strong> from <strong>{filename}</strong> and <strong>stop following anyone else</strong>.} other {You are about to <strong>follow</strong> up to <strong>{count} accounts</strong> from <strong>{filename}</strong> and <strong>stop following anyone else</strong>.}}"
            values={values}
          />
        )
      case 'blocking':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.blocking_html'
            defaultMessage="{count, plural, one {You are about to <strong>replace your block list</strong> with up to <strong>{count} account</strong> from <strong>{filename}</strong>.} other {You are about to <strong>replace your block list</strong> with up to <strong>{count} accounts</strong> from <strong>{filename}</strong>.}}"
            values={values}
          />
        )
      case 'muting':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.muting_html'
            defaultMessage="{count, plural, one {You are about to <strong>replace your list of muted account</strong> with up to <strong>{count} account</strong> from <strong>{filename}</strong>.} other {You are about to <strong>replace your list of muted accounts</strong> with up to <strong>{count} accounts</strong> from <strong>{filename}</strong>.}}"
            values={values}
          />
        )
      case 'domain_blocking':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.domain_blocking_html'
            defaultMessage="{count, plural, one {You are about to <strong>replace your domain block list</strong> with up to <strong>{count} domain</strong> from <strong>{filename}</strong>.} other {You are about to <strong>replace your domain block list</strong> with up to <strong>{count} domains</strong> from <strong>{filename}</strong>.}}"
            values={values}
          />
        )
      case 'bookmarks':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.bookmarks_html'
            defaultMessage="{count, plural, one {You are about to <strong>replace your bookmarks</strong> with up to <strong>{count} post</strong> from <strong>{filename}</strong>.} other {You are about to <strong>replace your bookmarks</strong> with up to <strong>{count} posts</strong> from <strong>{filename}</strong>.}}"
            values={values}
          />
        )
      case 'lists':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.lists_html'
            defaultMessage="{count, plural, one {You are about to <strong>replace your lists</strong> with contents of <strong>{filename}</strong>. Up to <strong>{count} account</strong> will be added to new lists.} other {You are about to <strong>replace your lists</strong> with contents of <strong>{filename}</strong>. Up to <strong>{count} accounts</strong> will be added to new lists.}}"
            values={values}
          />
        )
      case 'custom_filters':
        return (
          <FormattedMessage
            id='mastodon.imports.overwrite_preambles.custom_filters_html'
            defaultMessage="{count, plural, one {You are about to <strong>replace your filters</strong> with contents of <strong>{filename}</strong>. Up to <strong>{count} filter</strong> will be added to new filters.} other {You are about to <strong>replace your filters</strong> with contents of <strong>{filename}</strong>. Up to <strong>{count} filters</strong> will be added to new filters.}}"
            values={values}
          />
        )
    }
  }
  switch (pending.type) {
    case 'following':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.following_html'
          defaultMessage="{count, plural, one {You are about to <strong>follow</strong> up to <strong>{count} account</strong> from <strong>{filename}</strong>.} other {You are about to <strong>follow</strong> up to <strong>{count} accounts</strong> from <strong>{filename}</strong>.}}"
          values={values}
        />
      )
    case 'blocking':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.blocking_html'
          defaultMessage="{count, plural, one {You are about to <strong>block</strong> up to <strong>{count} account</strong> from <strong>{filename}</strong>.} other {You are about to <strong>block</strong> up to <strong>{count} accounts</strong> from <strong>{filename}</strong>.}}"
          values={values}
        />
      )
    case 'muting':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.muting_html'
          defaultMessage="{count, plural, one {You are about to <strong>mute</strong> up to <strong>{count} account</strong> from <strong>{filename}</strong>.} other {You are about to <strong>mute</strong> up to <strong>{count} accounts</strong> from <strong>{filename}</strong>.}}"
          values={values}
        />
      )
    case 'domain_blocking':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.domain_blocking_html'
          defaultMessage="{count, plural, one {You are about to <strong>block</strong> up to <strong>{count} domain</strong> from <strong>{filename}</strong>.} other {You are about to <strong>block</strong> up to <strong>{count} domains</strong> from <strong>{filename}</strong>.}}"
          values={values}
        />
      )
    case 'bookmarks':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.bookmarks_html'
          defaultMessage="{count, plural, one {You are about to add up to <strong>{count} post</strong> from <strong>{filename}</strong> to your <strong>bookmarks</strong>.} other {You are about to add up to <strong>{count} posts</strong> from <strong>{filename}</strong> to your <strong>bookmarks</strong>.}}"
          values={values}
        />
      )
    case 'lists':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.lists_html'
          defaultMessage="{count, plural, one {You are about to add up to <strong>{count} account</strong> from <strong>{filename}</strong> to your <strong>lists</strong>. New lists will be created if there is no list to add to.} other {You are about to add up to <strong>{count} accounts</strong> from <strong>{filename}</strong> to your <strong>lists</strong>. New lists will be created if there is no list to add to.}}"
          values={values}
        />
      )
    case 'custom_filters':
      return (
        <FormattedMessage
          id='mastodon.imports.preambles.custom_filters_html'
          defaultMessage="{count, plural, one {You are about to add up to <strong>{count} filter</strong> from <strong>{filename}</strong> to your <strong>filters</strong>.} other {You are about to add up to <strong>{count} filters</strong> from <strong>{filename}</strong> to your <strong>filters</strong>.}}"
          values={values}
        />
      )
  }

}

function Exports({ token }: { token: string }) {
  const [summary, setSummary] = useState<ExportSummary | null>(null)
  const [failed, setFailed] = useState(false)
  const [requesting, setRequesting] = useState(false)

  const load = useCallback(() => {
    getExportSummary(token)
      .then(setSummary)
      .catch(() => {
        setFailed(true)
        toast.error(t({ id: 'eunha.import_export.could_not_load_your_export', defaultMessage: 'Could not load your export' }))
      })
  }, [token])
  useEffect(load, [load])

  const download = async (file: string) => {
    try {
      await downloadFile(token, `/api/eunha/v1/exports/${file}`)
    } catch {
      toast.error(t({ id: 'eunha.import_export.could_not_download_the_file', defaultMessage: 'Could not download the file' }))
    }
  }

  const request = async () => {
    setRequesting(true)
    try {
      await requestBackup(token)
      load()
    } catch {
      toast.error(t({ id: 'eunha.import_export.could_not_request_your_archive', defaultMessage: 'Could not request your archive' }))
    } finally {
      setRequesting(false)
    }
  }

  const openBackup = async (id: string) => {
    try {
      window.location.assign(await getBackupDownloadUrl(token, id))
    } catch {
      toast.error(t({ id: 'eunha.import_export.could_not_download_your_archive', defaultMessage: 'Could not download your archive' }))
    }
  }

  if (!summary) return failed ? null : <SectionSkeleton rows={4} />

  return (
    <section className="space-y-4 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'eunha.import_export.data_export', defaultMessage: 'Data export' })}</h2>
      <Table>
        <TableBody>
          <TableRow>
            <TableHead>{t({ id: 'mastodon.exports.storage', defaultMessage: 'Media storage' })}</TableHead>
            <TableCell>{humanSize(summary.storage)}</TableCell>
            <TableCell />
          </TableRow>
          {EXPORTS.map(({ label, count, file }) => (
            <TableRow key={label}>
              <TableHead>{label}</TableHead>
              <TableCell>{Number(summary[count]).toLocaleString(currentLocale())}</TableCell>
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
        <FormattedMessage
          id='mastodon.exports.archive_takeout.hint_html'
          defaultMessage='You can request an archive of your <strong>posts and uploaded media</strong>. The exported data will be in the ActivityPub format, readable by any compliant software. You can request an archive every 7 days.'
          values={{ strong: (chunks) => <strong>{chunks}</strong> }}
        />
      </p>
      {summary.can_request_backup && (
        <Button size="sm" disabled={requesting} onClick={request}>
          {t({ id: 'mastodon.exports.archive_takeout.request', defaultMessage: 'Request your archive' })}
        </Button>
      )}
      {summary.backups.length > 0 && (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{t({ id: 'mastodon.sessions.date', defaultMessage: 'Date' })}</TableHead>
              <TableHead>{t({ id: 'mastodon.exports.archive_takeout.size', defaultMessage: 'Size' })}</TableHead>
              <TableHead />
            </TableRow>
          </TableHeader>
          <TableBody>
            {summary.backups.map((backup) => (
              <TableRow key={backup.id}>
                <TableCell>{new Date(backup.created_at).toLocaleString(currentLocale())}</TableCell>
                {backup.processed ? (
                  <>
                    <TableCell>{humanSize(backup.dump_file_size ?? 0)}</TableCell>
                    <TableCell className="text-right">
                      <Button variant="ghost" size="sm" onClick={() => openBackup(backup.id)}>
                        <Download /> {t({ id: 'mastodon.exports.archive_takeout.download', defaultMessage: 'Download your archive' })}
                      </Button>
                    </TableCell>
                  </>
                ) : (
                  <TableCell colSpan={2}>{t({ id: 'mastodon.exports.archive_takeout.in_progress', defaultMessage: 'Compiling your archive...' })}</TableCell>
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
      setError(e instanceof ApiError ? e.message : t({ id: 'eunha.import_export.could_not_upload_the_file', defaultMessage: 'Could not upload the file' }))
    } finally {
      setBusy(false)
    }
  }

  const confirm = async () => {
    if (!pending) return
    setBusy(true)
    try {
      await confirmImport(token, pending.id)
      toast.success(t({ id: 'mastodon.imports.success', defaultMessage: 'Your data was successfully uploaded and will be processed in due time' }))
      setPending(null)
      setFile(null)
      loadRecent()
    } catch {
      toast.error(t({ id: 'eunha.import_export.could_not_start_the_import', defaultMessage: 'Could not start the import' }))
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
      <h2 className="font-semibold">{t({ id: 'mastodon.settings.import', defaultMessage: 'Import' })}</h2>
      {pending ? (
        <div className="space-y-3">
          <p className="text-sm">{preamble(pending)}</p>
          {pending.likely_mismatched && (
            <p className="text-destructive text-sm">
              {t({ id: 'mastodon.imports.mismatched_types_warning', defaultMessage: 'It appears you may have selected the wrong type for this import, please double-check.' })}
            </p>
          )}
          {pending.missing_status && (
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.import_export.some_of_your_filters_hide_specific', defaultMessage: 'Some of your filters hide specific posts that are not known by this server. These posts will not be automatically filtered if they are discovered later by the server.' })}
            </p>
          )}
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={confirm}>
              {t({ id: 'mastodon.generic.confirm', defaultMessage: 'Confirm' })}
            </Button>
            <Button size="sm" variant="secondary" disabled={busy} onClick={cancel}>
              {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
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
            {t({ id: 'mastodon.imports.preface', defaultMessage: 'You can import data that you have exported from another server, such as a list of the people you are following or blocking.' })}
          </p>
          <div className="space-y-1">
            <Label>{t({ id: 'mastodon.imports.type', defaultMessage: 'Import type' })}</Label>
            <Select
              items={TYPES}
              value={type}
              onValueChange={(v) => setType((v as ImportType | null) ?? 'following')}
            >
              <SelectTrigger className="w-full" aria-label={t({ id: 'mastodon.imports.type', defaultMessage: 'Import type' })}>
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
            <Label htmlFor="import-file">{t({ id: 'mastodon.simple_form.labels.defaults.data', defaultMessage: 'Data' })}</Label>
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
              <RadioGroupItem value="merge" /> {t({ id: 'eunha.import_export.merge_keep_existing_records_and_add', defaultMessage: 'Merge — keep existing records and add new ones' })}
            </Label>
            <Label className="text-sm font-normal">
              <RadioGroupItem value="overwrite" /> {t({ id: 'eunha.import_export.overwrite_replace_current_records_with_the', defaultMessage: 'Overwrite — replace current records with the new ones' })}
            </Label>
          </RadioGroup>
          <Button type="submit" size="sm" disabled={!file || busy}>
            <Upload /> {t({ id: 'mastodon.imports.upload', defaultMessage: 'Upload' })}
          </Button>
        </form>
      )}

      {recent.length > 0 && (
        <>
          <h3 className="text-sm font-semibold">{t({ id: 'mastodon.imports.recent_imports', defaultMessage: 'Recent imports' })}</h3>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t({ id: 'mastodon.admin.software_updates.type', defaultMessage: 'Type' })}</TableHead>
                <TableHead>{t({ id: 'footer.status', defaultMessage: 'Status' })}</TableHead>
                <TableHead>{t({ id: 'mastodon.imports.time_started', defaultMessage: 'Started at' })}</TableHead>
                <TableHead>{t({ id: 'mastodon.imports.imported', defaultMessage: 'Imported' })}</TableHead>
                <TableHead>{t({ id: 'mastodon.imports.failures', defaultMessage: 'Failures' })}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {recent.map((r) => (
                <TableRow key={r.id}>
                  <TableCell>{TYPES[r.type]}</TableCell>
                  <TableCell>
                    <Badge variant="secondary">{STATES[r.state]}</Badge>
                  </TableCell>
                  <TableCell>{new Date(r.created_at).toLocaleString(currentLocale())}</TableCell>
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
                            () => toast.error(t({ id: 'eunha.import_export.could_not_download_the_failures', defaultMessage: 'Could not download the failures' })),
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
  return (
    <SettingsLayout to="/settings/export">
      {(token) => (
        <>
          <Exports token={token} />
          <Imports token={token} />
        </>
      )}
    </SettingsLayout>
  )
}
