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

/** `admin.server_checks.categories`, in the order the page lists them. */
const CATEGORIES: [ServerCheckCategory, string][] = [
  ['critical', 'Critical'],
  ['warning', 'Warnings'],
  ['suggestion', 'Suggestions'],
  ['error', 'Errors'],
  ['unprocessed', 'Unprocessed'],
  ['completed', 'Completed'],
  ['ignored', 'Ignored'],
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
    <AdminLayout title="Server health" permission="view_dashboard">
      <AdminError error={error} />
      <p className="text-muted-foreground text-sm">
        Recommendations to keep your server safe, welcoming and efficient.
      </p>
      {checks &&
        CATEGORIES.map(([category, label]) => {
          const inCategory = checks.filter((check) => check.category === category)
          if (inCategory.length === 0) return null
          return (
            <section key={category} className="mt-4 space-y-2">
              <h2 className="text-sm font-semibold">{label}</h2>
              {inCategory.map((check) => (
                <div key={check.key} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
                  <div className="min-w-0 flex-1 space-y-1">
                    <p className="text-sm font-medium">{check.title}</p>
                    {check.lead && (
                      <p className="text-muted-foreground text-sm break-words">{check.lead}</p>
                    )}
                    {check.checked_at && (
                      <p className="text-muted-foreground text-xs">
                        Last checked {formatDate(check.checked_at)}
                      </p>
                    )}
                  </div>
                  {check.setup_url &&
                    (check.setup_url.startsWith('/') ? (
                      <Button size="sm" render={<Link to={check.setup_url} />}>
                        Set up
                      </Button>
                    ) : (
                      <Button
                        size="sm"
                        render={<a href={check.setup_url} target="_blank" rel="noreferrer" />}
                      >
                        Set up
                      </Button>
                    ))}
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => void setIgnored(check.key, !check.ignored)}
                  >
                    {check.ignored ? 'Restore' : 'Ignore'}
                  </Button>
                </div>
              ))}
            </section>
          )
        })}
    </AdminLayout>
  )
}
