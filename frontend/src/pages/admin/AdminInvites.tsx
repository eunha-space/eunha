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
  const [pending, setPending] = useState<Set<string>>(new Set())
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
      <div className="mb-3 flex gap-3">
        <Button render={<Link to="/invites" />}>Create invite</Button>
        <Button variant="outline" render={<Link to="/invite-tree" />}>Invite tree</Button>
      </div>
      <AdminError error={error} />
      {invites === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {invites?.length === 0 && <p className="text-muted-foreground text-sm">No invites.</p>}
      <div className="space-y-2">
        {invites?.map((invite) => (
          <div key={invite.id} className="flex flex-wrap items-center gap-3 rounded-lg border p-3">
            <div className="min-w-0 flex-1 space-y-1">
              <div className="flex min-w-0 items-center gap-2">
                <a href={invite.url} className="min-w-0 truncate font-mono text-xs">{invite.url}</a>
                <Button size="xs" variant="secondary" disabled={!invite.valid_for_use} onClick={async () => {
                  try { await navigator.clipboard.writeText(invite.url); toast.success('Invite link copied') }
                  catch { toast.error('Could not copy link') }
                }}>Copy link</Button>
              </div>
              {invite.account && <AdminAccountLink account={invite.account} size="sm" />}
              {invite.comment && (
                <p className="text-muted-foreground text-xs">{invite.comment}</p>
              )}
            </div>
            <span className="text-muted-foreground text-xs tabular-nums">
              {invite.uses}
              {invite.max_uses !== null ? ` / ${invite.max_uses}` : ''} uses
            </span>
            {invite.valid_for_use && <Badge variant="outline">Available</Badge>}
            {invite.expired ? (
              <Badge variant="outline">Expired</Badge>
            ) : !invite.valid_for_use ? (
              <Badge variant="outline">{invite.max_uses !== null && invite.uses >= invite.max_uses ? 'Fully used' : 'Unavailable'}</Badge>
            ) : invite.expires_at ? (
              <span className="text-muted-foreground text-xs">
                Expires {formatDate(invite.expires_at)}
              </span>
            ) : (
              <span className="text-muted-foreground text-xs">Never expires</span>
            )}
            {invite.valid_for_use && (
              <Button
                size="xs"
                variant="outline"
                disabled={pending.has(invite.id)}
                onClick={async () => {
                  setPending(cur => new Set(cur).add(invite.id))
                  try {
                    await expireInvite(token ?? '', invite.id)
                    toast.success('Invite expired.')
                    load()
                  } catch (e) {
                    toast.error(errorMessage(e))
                  } finally {
                    setPending(cur => { const next = new Set(cur); next.delete(invite.id); return next })
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
