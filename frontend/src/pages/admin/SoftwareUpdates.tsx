import { useEffect, useState } from 'react'

import { AdminApiError } from '../../admin-api.ts'
import { listSoftwareUpdates, type SoftwareUpdate } from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

const TYPES: Record<SoftwareUpdate['type'], string> = {
  get patch() { return t({ id: 'eunha.admin.software_updates.patch_release', defaultMessage: 'Patch release' }) },
  get minor() { return t({ id: 'mastodon.admin.software_updates.types.minor', defaultMessage: 'Minor release' }) },
  get major() { return t({ id: 'mastodon.admin.software_updates.types.major', defaultMessage: 'Major release' }) },
}

/**
 * Software updates: Mastodon's `Admin::SoftwareUpdatesController`, the
 * releases newer than the Mastodon release eunha implements, as the optional
 * update check recorded them.
 */
export default function SoftwareUpdates() {
  const token = getToken()
  const [data, setData] = useState<{ current_version: string; updates: SoftwareUpdate[] } | null>(
    null,
  )
  const [disabled, setDisabled] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) return
    listSoftwareUpdates(token)
      .then(setData)
      .catch((e) => {
        if (e instanceof AdminApiError && e.status === 404) setDisabled(true)
        else setError(String(e))
      })
  }, [token])

  return (
    <AdminLayout title={t({ id: 'eunha.admin.software_updates.software_updates', defaultMessage: 'Software updates' })} permission="view_devops">
      <AdminError error={error} />
      {!data && !disabled && !error && <RowsSkeleton count={2} />}
      {disabled && (
        <p className="text-muted-foreground text-sm">
          <FormattedMessage
            id='eunha.admin.software_updates.turned_off'
            defaultMessage="The update check is turned off: this instance's configuration names no <code>software_update_url</code>."
            values={{ code: (chunks) => <code>{chunks}</code> }}
          />
        </p>
      )}
      {data && (
        <div className="space-y-3">
          <p className="text-muted-foreground text-sm">
            {t(
              { id: 'eunha.admin.software_updates.implements', defaultMessage: 'eunha implements Mastodon {version}. These are the newer releases the update server announced; adopting one takes a new eunha build.' },
              { version: data.current_version },
            )}
          </p>
          {data.updates.length === 0 && (
            <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.software_updates.there_are_no_newer_releases', defaultMessage: 'There are no newer releases.' })}</p>
          )}
          {data.updates.map((u) => (
            <div key={u.version} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
              <span className="font-medium">{u.version}</span>
              <Badge variant="outline">{TYPES[u.type]}</Badge>
              {u.urgent && <Badge variant="destructive">{t({ id: 'eunha.admin.software_updates.urgent', defaultMessage: 'Urgent' })}</Badge>}
              {u.end_of_support && (
                <span className="text-muted-foreground text-xs">
                  {t({ id: 'eunha.admin.software_updates.supported_until', defaultMessage: 'Supported until {date}' }, { date: u.end_of_support })}
                </span>
              )}
              {u.release_notes && (
                <a href={u.release_notes} target="_blank" rel="noreferrer" className="ml-auto text-sm">
                  {t({ id: 'mastodon.admin.software_updates.release_notes', defaultMessage: 'Release notes' })}
                </a>
              )}
            </div>
          ))}
        </div>
      )}
    </AdminLayout>
  )
}
