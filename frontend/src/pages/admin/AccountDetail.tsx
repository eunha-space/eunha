import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  can,
  createAccountNote,
  deleteAccount,
  deleteAccountNote,
  listAccountNotes,
  getAccount,
  rejectAccount,
  undoAccount,
  type AccountUndo,
  type AdminAccount,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import {
  AccountStateBadges,
  AdminAvatar,
  ConfirmButton,
  formatDate,
} from '@/components/admin/admin-common.tsx'
import { AccountActionDialog } from '@/components/admin/account-action-dialog.tsx'
import { ModerationNotes } from '@/components/admin/moderation-notes.tsx'
import { UserManagement } from '@/components/admin/user-management.tsx'
import { Button } from '@/components/ui/button.tsx'

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-0.5">
      <dt className="text-muted-foreground text-xs font-medium">{label}</dt>
      <dd className="text-sm break-words">{children}</dd>
    </div>
  )
}

// What each lifting call is called on the button, and what the toast says.
const UNDO: Record<AccountUndo, { label: string; done: string }> = {
  approve: { label: 'Approve', done: 'Approved.' },
  enable: { label: 'Unfreeze', done: 'Unfrozen.' },
  unsilence: { label: 'Undo limit', done: 'Limit lifted.' },
  unsuspend: { label: 'Undo suspension', done: 'Suspension lifted.' },
  unsensitive: { label: 'Undo force-sensitive', done: 'No longer force-sensitive.' },
}

/**
 * One account as a moderator sees it — Mastodon's admin account page on the
 * admin account API: the sign-in details only staff see, what has been done to
 * the account, and the actions that apply to it in its current state.
 */
export default function AccountDetail() {
  const { id = '' } = useParams()
  const navigate = useNavigate()
  const token = getToken()
  const permissions = useRolePermissions() ?? 0
  const [account, setAccount] = useState<AdminAccount | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [acting, setActing] = useState(false)

  const load = useCallback(() => {
    if (!token) return
    getAccount(token, id)
      .then((a) => {
        setAccount(a)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }, [token, id])

  useEffect(() => {
    load()
  }, [load])

  const undo = async (action: AccountUndo) => {
    if (!token || busy) return
    setBusy(true)
    try {
      setAccount(await undoAccount(token, id, action))
      toast.success(UNDO[action].done)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const a = account
  const pub = a?.account
  const local = a?.domain === null
  const pending = !!a && local && a.approved === false
  // Which lifting calls apply, given what has been done. Approval and
  // unfreezing need a local user, as upstream's `require_local_account!` says.
  const undos: AccountUndo[] = a
    ? ([
        pending && 'approve',
        local && a.disabled && 'enable',
        a.silenced && 'unsilence',
        a.suspended && 'unsuspend',
        a.sensitized && 'unsensitive',
      ].filter(Boolean) as AccountUndo[])
    : []

  return (
    <AdminLayout title={pub ? `@${pub.acct}` : 'Account'} permission="manage_users">
      <AdminError error={error} />
      {!a && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {a && pub && token && (
        <div className="space-y-5">
          <section className="flex items-start gap-3">
            <AdminAvatar account={pub} className="size-14" />
            <div className="min-w-0 flex-1 space-y-1">
              <div className="truncate font-semibold">{pub.display_name || pub.username}</div>
              <div className="text-muted-foreground truncate text-sm">@{pub.acct}</div>
              <AccountStateBadges account={a} />
            </div>
          </section>

          {pub.note && (
            <div
              className="text-sm [&_a]:text-primary [&_a]:underline"
              dangerouslySetInnerHTML={{ __html: pub.note }}
            />
          )}

          <section className="flex flex-wrap gap-2">
            {undos.map((u) => (
              <Button
                key={u}
                size="sm"
                variant={u === 'approve' ? 'default' : 'outline'}
                disabled={busy}
                onClick={() => void undo(u)}
              >
                {UNDO[u].label}
              </Button>
            ))}
            {pending && (
              <ConfirmButton
                title={`Reject @${pub.acct}?`}
                description="Rejecting a sign-up deletes the account. The username and email can be used again."
                confirmLabel="Reject"
                onConfirm={async () => {
                  await rejectAccount(token, id)
                  toast.success('Sign-up rejected.')
                  navigate('/admin/accounts?status=pending')
                }}
              >
                Reject
              </ConfirmButton>
            )}
            <Button size="sm" variant="destructive" onClick={() => setActing(true)}>
              Moderate…
            </Button>
            {/* Upstream's `AccountPolicy#destroy?`: only a suspended account,
                and only for a role that may delete user data. */}
            {a.suspended && can(permissions, 'delete_user_data') && (
              <ConfirmButton
                title={`Delete @${pub.acct} permanently?`}
                description="Everything the account has posted and stored is deleted. This cannot be undone, and the suspension can no longer be lifted."
                confirmLabel="Delete permanently"
                onConfirm={async () => {
                  await deleteAccount(token, id)
                  toast.success('Deletion queued.')
                  load()
                }}
              >
                Delete permanently
              </ConfirmButton>
            )}
          </section>

          <dl className="grid gap-4 sm:grid-cols-2">
            {local ? (
              <>
                <Field label="Email">
                  {a.email ?? '—'}
                  {a.email && !a.confirmed && (
                    <span className="text-muted-foreground"> (unconfirmed)</span>
                  )}
                </Field>
                <Field label="Role">{a.role?.name || 'No role'}</Field>
                <Field label="Most recent IP">{a.ip ?? '—'}</Field>
                <Field label="Locale">{a.locale ?? '—'}</Field>
                {a.invite_request && (
                  <Field label="Reason for joining">{a.invite_request}</Field>
                )}
                {a.invited_by_account_id && (
                  <Field label="Invited by">
                    <Link to={`/admin/accounts/${a.invited_by_account_id}`}>
                      Account {a.invited_by_account_id}
                    </Link>
                  </Field>
                )}
              </>
            ) : (
              <Field label="Domain">{a.domain}</Field>
            )}
            <Field label="Joined">{formatDate(a.created_at)}</Field>
            <Field label="Profile">
              <Link to={`/@${pub.acct}`}>View profile</Link>
              {pub.url && !local && (
                <>
                  {' · '}
                  <a href={pub.url} target="_blank" rel="noreferrer">
                    On {a.domain}
                  </a>
                </>
              )}
            </Field>
            <Field label="Posts">
              {pub.statuses_count ?? 0} · {pub.followers_count ?? 0} followers ·{' '}
              {pub.following_count ?? 0} following
            </Field>
            <Field label="Reports">
              <Link to={`/admin/reports?target_account_id=${a.id}`}>About this account</Link>
              {' · '}
              <Link to={`/admin/reports?account_id=${a.id}`}>Filed by this account</Link>
            </Field>
            <Field label="Moderation">
              <Link to={`/admin/accounts/${a.id}/statuses`}>Posts</Link>
              {' · '}
              <Link to={`/admin/accounts/${a.id}/relationships`}>Relationships</Link>
              {can(permissions, 'view_audit_log') && (
                <>
                  {' · '}
                  <Link to={`/admin/action_logs?target_account_id=${a.id}`}>Audit log</Link>
                </>
              )}
            </Field>
          </dl>

          {local && a.ips && a.ips.length > 0 && (
            <section className="space-y-1">
              <h2 className="text-sm font-semibold">Recent IPs</h2>
              <ul className="text-sm">
                {a.ips.map((ip) => (
                  <li key={`${ip.ip}-${ip.used_at}`} className="flex justify-between gap-2">
                    <Link to={`/admin/accounts?ip=${encodeURIComponent(ip.ip)}`}>{ip.ip}</Link>
                    <span className="text-muted-foreground text-xs">{formatDate(ip.used_at)}</span>
                  </li>
                ))}
              </ul>
            </section>
          )}

          {local && a.approved !== null && (
            <UserManagement
              account={a}
              permissions={permissions}
              token={token}
              onChanged={setAccount}
            />
          )}

          {can(permissions, 'manage_reports') && (
            <ModerationNotes
              key={a.id}
              load={() => listAccountNotes(token, a.id)}
              create={(content) => createAccountNote(token, a.id, content)}
              remove={(noteId) => deleteAccountNote(token, noteId)}
            />
          )}

          <AccountActionDialog
            account={a}
            token={token}
            open={acting}
            onOpenChange={setActing}
            onDone={load}
          />
        </div>
      )}
    </AdminLayout>
  )
}
