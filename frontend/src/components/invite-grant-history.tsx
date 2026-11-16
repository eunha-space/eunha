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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'

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
      .catch(() => { if (live) setError(t({ id: 'eunha.invite_grant_history.could_not_load_handouts', defaultMessage: 'Could not load handouts.' })) })
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
    <section className="mb-6 space-y-2" aria-label={t({ id: 'eunha.invite_grant_history.handouts', defaultMessage: 'Handouts' })}>
      <h2 className="text-base font-semibold">{t({ id: 'eunha.invite_grant_history.handouts', defaultMessage: 'Handouts' })}</h2>
      {error && (
        <p role="alert" className="text-destructive text-sm">
          {error}{' '}
          <Button size="xs" variant="outline" onClick={() => setReload((n) => n + 1)}>{t({ id: 'bundle_column_error.retry', defaultMessage: 'Try again' })}</Button>
        </p>
      )}
      {grants === null && !error && <RowsSkeleton count={2} />}
      {grants?.length === 0 && <p className="text-muted-foreground text-sm">{t({ id: 'eunha.invite_grant_history.no_invites_handed_out_yet', defaultMessage: 'No invites handed out yet.' })}</p>}
      {grants?.map((grant) => (
        <div key={grant.id} className="flex flex-wrap items-center gap-3 rounded-lg border p-3 text-sm">
          <div className="min-w-0 flex-1 space-y-1">
            <div className="font-medium">
              {grant.recipient
                ? t(
                    { id: 'eunha.invite_grant_history.links_to_member', defaultMessage: '{links, plural, one {# link} other {# links}} to @{recipient}' },
                    { links: grant.links, recipient: grant.recipient },
                  )
                : t(
                    { id: 'eunha.invite_grant_history.links_to_members', defaultMessage: '{links, plural, one {# link} other {# links}} to {recipients} members' },
                    { links: grant.links, recipients: grant.recipients },
                  )}
            </div>
            <div className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1 text-xs">
              <span>
                {grant.granted_by
                  ? t({ id: 'eunha.invite_grant_history.by', defaultMessage: 'By @{acct}' }, { acct: grant.granted_by })
                  : t({ id: 'eunha.invite_grant_history.by_former_staff', defaultMessage: 'By former staff' })} ·{' '}
                {inviteDate(grant.created_at).toLocaleString(currentLocale())}
              </span>
              <span>
                {t(
                  { id: 'eunha.invite_grant_history.usage', defaultMessage: '{used} used · {available} available' },
                  { used: grant.uses, available: grant.available },
                )}
              </span>
              {grant.expires_at && !grant.revoked_at && (
                <span>
                  {t(
                    { id: 'eunha.invites.expires_on', defaultMessage: 'Expires {date}' },
                    { date: inviteDate(grant.expires_at).toLocaleString(currentLocale()) },
                  )}
                </span>
              )}
              {grant.revoked_at && (
                <Badge variant="outline">
                  {grant.revoked_by
                    ? t(
                        { id: 'eunha.invite_grant_history.revoked_by', defaultMessage: 'Revoked by @{acct} {date}' },
                        { acct: grant.revoked_by, date: inviteDate(grant.revoked_at).toLocaleString(currentLocale()) },
                      )
                    : t(
                        { id: 'eunha.invite_grant_history.revoked', defaultMessage: 'Revoked {date}' },
                        { date: inviteDate(grant.revoked_at).toLocaleString(currentLocale()) },
                      )}
                </Badge>
              )}
            </div>
          </div>
          {!grant.revoked_at && grant.available > 0 && (
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.invite_grant_history.revoke_this_handout', defaultMessage: 'Revoke this handout?' })}
              description={t(
                {
                  id: 'eunha.invite_grant_history.revoke_description',
                  defaultMessage: '{count, plural, one {# unused link expires} other {# unused links expire}} now. People who already joined stay, and the links stay listed.',
                },
                { count: grant.available },
              )}
              confirmLabel={t({ id: 'mastodon.sessions.revoke', defaultMessage: 'Revoke' })}
              onConfirm={async () => {
                const { expired } = await revokeInviteGrant(token ?? '', grant.id)
                toast.success(
                  t({ id: 'eunha.invite_grant_history.expired', defaultMessage: '{count, plural, one {# link} other {# links}} expired.' }, { count: expired }),
                )
                setReload((n) => n + 1)
              }}
            >
              {t({ id: 'mastodon.sessions.revoke', defaultMessage: 'Revoke' })}
            </ConfirmButton>
          )}
        </div>
      ))}
      {more && <Button size="sm" variant="outline" onClick={loadMore}>{t({ id: 'eunha.invite_grant_history.older_handouts', defaultMessage: 'Older handouts' })}</Button>}
    </section>
  )
}
