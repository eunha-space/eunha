import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import { getAccountStatus, type AdminStatusDetail } from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminStatus, formatDate } from '@/components/admin/admin-common.tsx'

/**
 * One post as a moderator sees it, with every version it has had: Mastodon's
 * `Admin::StatusesController#show`.
 */
export default function AccountStatusDetail() {
  const { id = '', statusId = '' } = useParams()
  const token = getToken()
  const [status, setStatus] = useState<AdminStatusDetail | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) return
    getAccountStatus(token, id, statusId)
      .then(setStatus)
      .catch((e) => setError(String(e)))
  }, [token, id, statusId])

  return (
    <AdminLayout title="Post" permission="manage_users">
      <p className="mb-3 text-sm">
        <Link to={`/admin/accounts/${id}/statuses`}>Back to the account's posts</Link>
      </p>
      <AdminError error={error} />
      {!status && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {status && (
        <div className="space-y-4">
          <AdminStatus status={status} />
          <section className="space-y-2">
            <h2 className="text-sm font-semibold">Version history</h2>
            {status.edits.length <= 1 && (
              <p className="text-muted-foreground text-sm">This post has not been edited.</p>
            )}
            {status.edits.length > 1 &&
              status.edits.map((edit, i) => (
                <article key={i} className="space-y-1 rounded-lg border p-3">
                  <div className="text-muted-foreground text-xs">
                    {formatDate(edit.created_at)}
                    {edit.sensitive && ' · sensitive'}
                  </div>
                  {edit.spoiler_text && (
                    <p className="text-sm font-medium">CW: {edit.spoiler_text}</p>
                  )}
                  <div
                    className="text-sm break-words [&_a]:text-primary [&_a]:underline"
                    dangerouslySetInnerHTML={{ __html: edit.content }}
                  />
                </article>
              ))}
          </section>
        </div>
      )}
    </AdminLayout>
  )
}
