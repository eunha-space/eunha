import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  getInviteGrants,
  revokeInviteGrant,
  type InviteGrantSummary,
} from '../eunha-api.ts'
import { getToken } from '../auth.ts'
import { inviteDate } from '@/lib/invites.ts'
import { errorMessage } from '@/lib/utils.ts'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'

/**
 * Every staff handout, newest first: who gave it, to whom, and what became of
 * its links. Revoking expires the links still usable and keeps their history.
 */
export function InviteGrantHistory({ refresh = 0 }: { refresh?: number }) {
  const token = getToken()
  const [grants, setGrants] = useState<InviteGrantSummary[] | null>(null)
  const [more, setMore] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)

  useEffect(() => {
    if (!token) return
    let live = true
    getInviteGrants(token)
      .then((list) => {
        if (!live) return
        setGrants(list)
        setMore(list.length === 40)
        setError(null)
      })
      .catch(() => { if (live) setError('Could not load handouts.') })
    return () => { live = false }
  }, [token, refresh, reload])

  const loadMore = async () => {
    if (!token || !grants?.length) return
    try {
      const next = await getInviteGrants(token, grants[grants.length - 1].id)
      setGrants([...grants, ...next])
      setMore(next.length === 40)
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <section className="mb-6 space-y-2" aria-label="Handouts">
      <h2 className="text-base font-semibold">Handouts</h2>
      {error && (
        <p role="alert" className="text-destructive text-sm">
          {error}{' '}
          <Button size="xs" variant="outline" onClick={() => setReload((n) => n + 1)}>Try again</Button>
        </p>
      )}
      {grants === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {grants?.length === 0 && <p className="text-muted-foreground text-sm">No invites handed out yet.</p>}
      {grants?.map((grant) => (
        <div key={grant.id} className="flex flex-wrap items-center gap-3 rounded-lg border p-3 text-sm">
          <div className="min-w-0 flex-1 space-y-1">
            <div className="font-medium">
              {grant.links} {grant.links === 1 ? 'link' : 'links'} to{' '}
              {grant.recipient ? `@${grant.recipient}` : `${grant.recipients} members`}
            </div>
            <div className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1 text-xs">
              <span>
                {grant.granted_by ? `By @${grant.granted_by}` : 'By former staff'} ·{' '}
                {inviteDate(grant.created_at).toLocaleString()}
              </span>
              <span>{grant.uses} used · {grant.available} available</span>
              {grant.expires_at && !grant.revoked_at && (
                <span>Expires {inviteDate(grant.expires_at).toLocaleString()}</span>
              )}
              {grant.revoked_at && (
                <Badge variant="outline">
                  Revoked{grant.revoked_by ? ` by @${grant.revoked_by}` : ''}{' '}
                  {inviteDate(grant.revoked_at).toLocaleString()}
                </Badge>
              )}
            </div>
          </div>
          {!grant.revoked_at && grant.available > 0 && (
            <ConfirmButton
              size="xs"
              title="Revoke this handout?"
              description={`${grant.available} unused ${grant.available === 1 ? 'link expires' : 'links expire'} now. People who already joined stay, and the links stay listed.`}
              confirmLabel="Revoke"
              onConfirm={async () => {
                const { expired } = await revokeInviteGrant(token ?? '', grant.id)
                toast.success(`${expired} ${expired === 1 ? 'link' : 'links'} expired.`)
                setReload((n) => n + 1)
              }}
            >
              Revoke
            </ConfirmButton>
          )}
        </div>
      ))}
      {more && <Button size="sm" variant="outline" onClick={loadMore}>Older handouts</Button>}
    </section>
  )
}
