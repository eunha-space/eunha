import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import {
  deactivateAllInvites,
  expireInvite,
  listAdminInvites,
  type AdminInvite,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import {
  AdminAccountLink,
  ChoiceSelect,
  ConfirmButton,
  formatDate,
} from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'

const FILTERS = { all: 'All', available: 'Available', expired: 'Expired' }

/**
 * Every invite on the server: Mastodon's `Admin::InvitesController`, for a
 * role with `manage_invites`. New invites are made on the invite page.
 */
export default function AdminInvites() {
  const token = getToken()
  const [filter, setFilter] = useState<keyof typeof FILTERS>('all')
  const [page, setPage] = useState(1)
  const [invites, setInvites] = useState<AdminInvite[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = () => {
    if (!token) return
    listAdminInvites(token, filter === 'all' ? undefined : filter, page)
      .then((list) => {
        setInvites(list)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token, filter, page])

  return (
    <AdminLayout
      title="Invites"
      permission="manage_invites"
      actions={
        <>
          <ChoiceSelect
            label="Show"
            value={filter}
            items={FILTERS}
            onChange={(v) => {
              setFilter(v)
              setPage(1)
            }}
            className="w-36"
          />
          <ConfirmButton
            title="Deactivate all invite links?"
            description="Every invite that can still be used expires now."
            confirmLabel="Deactivate all"
            onConfirm={async () => {
              await deactivateAllInvites(token ?? '')
              toast.success('All invite links deactivated.')
              load()
            }}
          >
            Deactivate all
          </ConfirmButton>
        </>
      }
    >
      <p className="text-muted-foreground mb-3 text-sm">
        New invites are made on the <Link to="/invites">invite page</Link>.
      </p>
      <AdminError error={error} />
      {invites === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {invites?.length === 0 && <p className="text-muted-foreground text-sm">No invites.</p>}
      <div className="space-y-2">
        {invites?.map((invite) => (
          <div key={invite.id} className="flex flex-wrap items-center gap-3 rounded-lg border p-3">
            <div className="min-w-0 flex-1 space-y-1">
              <code className="text-sm">{invite.code}</code>
              {invite.account && <AdminAccountLink account={invite.account} size="sm" />}
              {invite.comment && (
                <p className="text-muted-foreground text-xs">{invite.comment}</p>
              )}
            </div>
            <span className="text-muted-foreground text-xs tabular-nums">
              {invite.uses}
              {invite.max_uses !== null ? ` / ${invite.max_uses}` : ''} uses
            </span>
            {invite.expired ? (
              <Badge variant="outline">Expired</Badge>
            ) : invite.expires_at ? (
              <span className="text-muted-foreground text-xs">
                Expires {formatDate(invite.expires_at)}
              </span>
            ) : (
              <span className="text-muted-foreground text-xs">Never expires</span>
            )}
            {!invite.expired && (
              <Button
                size="xs"
                variant="outline"
                onClick={async () => {
                  try {
                    await expireInvite(token ?? '', invite.id)
                    toast.success('Invite expired.')
                    load()
                  } catch (e) {
                    toast.error(errorMessage(e))
                  }
                }}
              >
                Expire
              </Button>
            )}
          </div>
        ))}
      </div>
      <div className="mt-2 flex gap-2">
        <Button size="sm" variant="outline" disabled={page === 1} onClick={() => setPage(page - 1)}>
          Newer
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={(invites?.length ?? 0) < 40}
          onClick={() => setPage(page + 1)}
        >
          Older
        </Button>
      </div>
    </AdminLayout>
  )
}
