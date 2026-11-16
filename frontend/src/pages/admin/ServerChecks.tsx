import { useCallback, useEffect, useState } from 'react'
import { toast } from 'sonner'
import { Link } from 'react-router-dom'

import {
  listServerChecks,
  setServerCheckIgnored,
  type ServerCheck,
  type ServerCheckCategory,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { formatDate } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import type { MessageDescriptor } from 'react-intl'

/** `admin.server_checks.categories`, in the order the page lists them. */
const CATEGORIES: [ServerCheckCategory, MessageDescriptor][] = [
  ['critical', { id: 'mastodon.admin.server_checks.categories.critical.title', defaultMessage: 'Critical' }],
  ['warning', { id: 'mastodon.admin.server_checks.categories.warning.title', defaultMessage: 'Warnings' }],
  ['suggestion', { id: 'mastodon.admin.server_checks.categories.suggestion.title', defaultMessage: 'Suggestions' }],
  ['error', { id: 'mastodon.admin.server_checks.categories.error.title', defaultMessage: 'Errors' }],
  ['unprocessed', { id: 'mastodon.admin.server_checks.categories.unprocessed.title', defaultMessage: 'Unprocessed' }],
  ['completed', { id: 'mastodon.admin.server_checks.categories.completed.title', defaultMessage: 'Completed' }],
  ['ignored', { id: 'mastodon.admin.server_checks.categories.ignored.title', defaultMessage: 'Ignored' }],
]

/** `Admin::ServerChecksController#index`. */
export default function ServerChecks() {
  const token = getToken()
  const [checks, setChecks] = useState<ServerCheck[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(() => {
    if (!token) return
    listServerChecks(token)
      .then(setChecks)
      .catch((e) => setError(errorMessage(e)))
  }, [token])

  useEffect(load, [load])

  const setIgnored = async (key: string, ignored: boolean) => {
    try {
      await setServerCheckIgnored(token ?? '', key, ignored)
      load()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title={t({ id: 'eunha.admin.server_checks.server_health', defaultMessage: 'Server health' })} permission="view_dashboard">
      <AdminError error={error} />
      <p className="text-muted-foreground text-sm">
        {t({ id: 'eunha.admin.server_checks.recommendations_to_keep_your_server_safe', defaultMessage: 'Recommendations to keep your server safe, welcoming and efficient.' })}
      </p>
      {checks === null && !error && <RowsSkeleton action />}
      {checks &&
        CATEGORIES.map(([category, label]) => {
          const inCategory = checks.filter((check) => check.category === category)
          if (inCategory.length === 0) return null
          return (
            <section key={category} className="mt-4 space-y-2">
              <h2 className="text-sm font-semibold">{t(label)}</h2>
              {inCategory.map((check) => (
                <div key={check.key} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
                  <div className="min-w-0 flex-1 space-y-1">
                    <p className="text-sm font-medium">{check.title}</p>
                    {check.lead && (
                      <p className="text-muted-foreground text-sm break-words">{check.lead}</p>
                    )}
                    {check.checked_at && (
                      <p className="text-muted-foreground text-xs">
                        {t({ id: 'eunha.admin.server_checks.last_checked', defaultMessage: 'Last checked {date}' }, { date: formatDate(check.checked_at) })}
                      </p>
                    )}
                  </div>
                  {check.setup_url &&
                    (check.setup_url.startsWith('/') ? (
                      <Button size="sm" render={<Link to={check.setup_url} />}>
                        {t({ id: 'mastodon.otp_authentication.setup', defaultMessage: 'Set up' })}
                      </Button>
                    ) : (
                      <Button
                        size="sm"
                        render={<a href={check.setup_url} target="_blank" rel="noreferrer" />}
                      >
                        {t({ id: 'mastodon.otp_authentication.setup', defaultMessage: 'Set up' })}
                      </Button>
                    ))}
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => void setIgnored(check.key, !check.ignored)}
                  >
                    {check.ignored ? t({ id: 'eunha.admin.server_checks.restore', defaultMessage: 'Restore' }) : t({ id: 'notifications.policy.drop', defaultMessage: 'Ignore' })}
                  </Button>
                </div>
              ))}
            </section>
          )
        })}
    </AdminLayout>
  )
}
