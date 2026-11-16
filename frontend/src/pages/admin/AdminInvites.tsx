import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import {
  deactivateAllInvites,
  expireInvite,
  listAdminInvites,
  type AdminInvite,
} from '../../admin-server-api.ts'
import { InviteGrantForm } from '@/components/invite-grant-form.tsx'
import { InviteGrantHistory } from '@/components/invite-grant-history.tsx'
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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const FILTERS = { get all() { return t({ id: 'notifications.filter.all', defaultMessage: 'All' }) }, get available() { return t({ id: 'mastodon.admin.invites.filter.available', defaultMessage: 'Available' }) }, get expired() { return t({ id: 'filter_modal.select_filter.expired', defaultMessage: 'Expired' }) } }

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
  const [granted, setGranted] = useState(0)

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
      title={t({ id: 'mastodon.admin.invites.title', defaultMessage: 'Invites' })}
      permission="manage_invites"
      actions={
        <>
          <ChoiceSelect
            label={t({ id: 'content_warning.show_short', defaultMessage: 'Show' })}
            value={filter}
            items={FILTERS}
            onChange={(v) => {
              setFilter(v)
              setPage(1)
            }}
            className="w-36"
          />
          <ConfirmButton
            title={t({ id: 'eunha.admin.admin_invites.deactivate_all_invite_links', defaultMessage: 'Deactivate all invite links?' })}
            description={t({ id: 'eunha.admin.admin_invites.every_invite_that_can_still_be', defaultMessage: 'Every invite that can still be used expires now.' })}
            confirmLabel={t({ id: 'mastodon.admin.invites.deactivate_all', defaultMessage: 'Deactivate all' })}
            onConfirm={async () => {
              await deactivateAllInvites(token ?? '')
              toast.success(t({ id: 'eunha.admin.admin_invites.all_invite_links_deactivated', defaultMessage: 'All invite links deactivated.' }))
              load()
            }}
          >
            {t({ id: 'mastodon.admin.invites.deactivate_all', defaultMessage: 'Deactivate all' })}
          </ConfirmButton>
        </>
      }
    >
      <InviteGrantForm onGranted={() => { load(); setGranted((n) => n + 1) }} />
      <InviteGrantHistory refresh={granted} />
      <div className="mb-3 flex gap-3">
        <Button render={<Link to="/invites" />}>{t({ id: 'eunha.admin.admin_invites.create_invite', defaultMessage: 'Create invite' })}</Button>
        <Button variant="outline" render={<Link to="/invite-tree" />}>{t({ id: 'eunha.admin.admin_invites.invite_tree', defaultMessage: 'Invite tree' })}</Button>
      </div>
      <AdminError error={error} />
      {invites === null && !error && <RowsSkeleton action />}
      {invites?.length === 0 && <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.admin_invites.no_invites', defaultMessage: 'No invites.' })}</p>}
      <div className="space-y-2">
        {invites?.map((invite) => (
          <div key={invite.id} className="flex flex-wrap items-center gap-3 rounded-lg border p-3">
            <div className="min-w-0 flex-1 space-y-1">
              <div className="flex min-w-0 items-center gap-2">
                <a href={invite.url} className="min-w-0 truncate font-mono text-xs">{invite.url}</a>
                <Button size="xs" variant="secondary" disabled={!invite.valid_for_use} onClick={async () => {
                  try { await navigator.clipboard.writeText(invite.url); toast.success(t({ id: 'eunha.admin.admin_invites.invite_link_copied', defaultMessage: 'Invite link copied' })) }
                  catch { toast.error(t({ id: 'eunha.admin.admin_invites.could_not_copy_link', defaultMessage: 'Could not copy link' })) }
                }}>{t({ id: 'account.menu.copy', defaultMessage: 'Copy link' })}</Button>
              </div>
              {invite.account && <AdminAccountLink account={invite.account} size="sm" />}
              {invite.comment && (
                <p className="text-muted-foreground text-xs">{invite.comment}</p>
              )}
            </div>
            <span className="text-muted-foreground text-xs tabular-nums">
              {invite.max_uses !== null
                ? t({ id: 'eunha.admin.admin_invites.uses_of_max', defaultMessage: '{uses} / {max} uses' }, { uses: invite.uses, max: invite.max_uses })
                : t({ id: 'eunha.admin.admin_invites.uses', defaultMessage: '{uses} uses' }, { uses: invite.uses })}
            </span>
            {invite.valid_for_use && <Badge variant="outline">{t({ id: 'mastodon.admin.invites.filter.available', defaultMessage: 'Available' })}</Badge>}
            {invite.expired ? (
              <Badge variant="outline">{t({ id: 'filter_modal.select_filter.expired', defaultMessage: 'Expired' })}</Badge>
            ) : !invite.valid_for_use ? (
              <Badge variant="outline">{invite.max_uses !== null && invite.uses >= invite.max_uses ? t({ id: 'eunha.admin.admin_invites.fully_used', defaultMessage: 'Fully used' }) : t({ id: 'mastodon.admin.instances.delivery.unavailable', defaultMessage: 'Unavailable' })}</Badge>
            ) : invite.expires_at ? (
              <span className="text-muted-foreground text-xs">
                {t({ id: 'eunha.invites.expires_on', defaultMessage: 'Expires {date}' }, { date: formatDate(invite.expires_at) })}
              </span>
            ) : (
              <span className="text-muted-foreground text-xs">{t({ id: 'eunha.admin.admin_invites.never_expires', defaultMessage: 'Never expires' })}</span>
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
                    toast.success(t({ id: 'eunha.admin.admin_invites.invite_expired', defaultMessage: 'Invite expired.' }))
                    load()
                  } catch (e) {
                    toast.error(errorMessage(e))
                  } finally {
                    setPending(cur => { const next = new Set(cur); next.delete(invite.id); return next })
                  }
                }}
              >
                {t({ id: 'eunha.admin.admin_invites.expire', defaultMessage: 'Expire' })}
              </Button>
            )}
          </div>
        ))}
      </div>
      <div className="mt-2 flex gap-2">
        <Button size="sm" variant="outline" disabled={page === 1} onClick={() => setPage(page - 1)}>
          {t({ id: 'mastodon.pagination.newer', defaultMessage: 'Newer' })}
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={(invites?.length ?? 0) < 40}
          onClick={() => setPage(page + 1)}
        >
          {t({ id: 'mastodon.pagination.older', defaultMessage: 'Older' })}
        </Button>
      </div>
    </AdminLayout>
  )
}
