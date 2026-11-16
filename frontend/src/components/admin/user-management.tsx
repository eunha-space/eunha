import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  can,
  changeUserEmail,
  changeUserRole,
  disableTwoFactor,
  listAssignableRoles,
  userAccessAction,
  type AdminAccount,
  type AssignableRole,
} from '../../admin-api.ts'
import { getCurrentAccount } from '../../api.ts'
import { errorMessage } from '@/lib/utils.ts'
import { ChoiceSelect, ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog.tsx'
import { Input } from '@/components/ui/input.tsx'
import { t } from '@/i18n.tsx'

const NO_ROLE = 'none'

/**
 * What a moderator may do to a local user's sign-in, each behind the
 * permission Mastodon's `UserPolicy` asks for: change the role
 * (`manage_roles`), and with `manage_user_access` reset the password, turn
 * two-factor authentication off, change the email address, and confirm it.
 * The server also asks that the moderator's role outrank the user's.
 */
export function UserManagement({
  account,
  permissions,
  token,
  onChanged,
}: {
  account: AdminAccount
  permissions: number
  token: string
  onChanged: (account: AdminAccount) => void
}) {
  const manageRoles = can(permissions, 'manage_roles')
  const manageAccess = can(permissions, 'manage_user_access')
  const [actingRoleId, setActingRoleId] = useState<string | null>(null)
  const [roles, setRoles] = useState<AssignableRole[]>([])
  const [roleId, setRoleId] = useState<string>(NO_ROLE)
  const [emailOpen, setEmailOpen] = useState(false)
  const [email, setEmail] = useState('')
  const [busy, setBusy] = useState(false)
  const currentRole = account.role && account.role.id !== '-99' ? account.role.id : NO_ROLE

  useEffect(() => {
    setRoleId(currentRole)
  }, [currentRole])

  useEffect(() => {
    if (!manageRoles) return
    let cancelled = false
    Promise.all([listAssignableRoles(token), getCurrentAccount(token)]).then(([roles, me]) => {
      if (cancelled) return
      setRoles(roles)
      setActingRoleId((me as unknown as { role?: { id: string } }).role?.id ?? '-99')
    }).catch(() => {})
    return () => { cancelled = true }
  }, [manageRoles, token])

  if (!manageRoles && !manageAccess) return null

  const run = async (work: () => Promise<AdminAccount>, done: string) => {
    if (busy) return
    setBusy(true)
    try {
      onChanged(await work())
      toast.success(done)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const actingPosition = roles.find((r) => r.id === actingRoleId)?.position
  const targetPosition = currentRole === NO_ROLE ? -1 : roles.find((r) => r.id === currentRole)?.position
  const canChangeRole = manageRoles && actingPosition !== undefined && targetPosition !== undefined
    && actingPosition > targetPosition

  const roleItems: Record<string, string> = { [NO_ROLE]: t({ id: 'mastodon.admin.accounts.change_role.no_role', defaultMessage: 'No role' }) }
  for (const r of roles) {
    if (actingPosition !== undefined && r.position <= actingPosition) roleItems[r.id] = r.name
  }

  const submitEmail = async (e: FormEvent) => {
    e.preventDefault()
    await run(() => changeUserEmail(token, account.id, email.trim()), t({ id: 'eunha.admin.user_management.confirmation_link_sent_to_the_new', defaultMessage: 'Confirmation link sent to the new address.' }))
    setEmailOpen(false)
  }

  return (
    <section className="space-y-2">
      <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.user_management.sign_in_and_role', defaultMessage: 'Sign-in and role' })}</h2>
      {manageRoles && actingPosition !== undefined && targetPosition !== undefined && !canChangeRole && (
        <p className="text-muted-foreground text-sm">
          {t({ id: 'eunha.admin.user_management.you_can_only_change_roles_for', defaultMessage: 'You can only change roles for users whose role is below yours.' })}
        </p>
      )}
      {canChangeRole && (
        <div className="flex flex-wrap items-center gap-2">
          <ChoiceSelect
            label={t({ id: 'mastodon.admin.accounts.role', defaultMessage: 'Role' })}
            value={roleId}
            items={roleItems}
            onChange={setRoleId}
            className="w-full sm:w-56"
          />
          <Button
            size="sm"
            variant="outline"
            disabled={busy || roleId === currentRole}
            onClick={() =>
              void run(
                () => changeUserRole(token, account.id, roleId === NO_ROLE ? null : roleId),
                t({ id: 'eunha.admin.user_management.role_changed', defaultMessage: 'Role changed.' }),
              )
            }
          >
            {t({ id: 'mastodon.admin.accounts.change_role.label', defaultMessage: 'Change role' })}
          </Button>
        </div>
      )}
      {manageAccess && (
        <div className="flex flex-wrap gap-2">
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={() => {
              setEmail(account.email ?? '')
              setEmailOpen(true)
            }}
          >
            {t({ id: 'mastodon.admin.accounts.change_email.label', defaultMessage: 'Change email' })}
          </Button>
          {!account.confirmed && (
            <>
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                onClick={() =>
                  void run(() => userAccessAction(token, account.id, 'confirmation'), t({ id: 'eunha.common.confirmed', defaultMessage: 'Confirmed.' }))
                }
              >
                {t({ id: 'mastodon.admin.accounts.confirm', defaultMessage: 'Confirm' })}
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                onClick={() =>
                  void run(
                    () => userAccessAction(token, account.id, 'confirmation/resend'),
                    t({ id: 'mastodon.admin.accounts.resend_confirmation.success', defaultMessage: 'Confirmation link successfully sent!' }),
                  )
                }
              >
                {t({ id: 'mastodon.admin.accounts.resend_confirmation.send', defaultMessage: 'Resend confirmation link' })}
              </Button>
            </>
          )}
          <ConfirmButton
            title={t({ id: 'eunha.admin.user_management.reset_the_password', defaultMessage: 'Reset the password?' })}
            description={t({ id: 'eunha.admin.user_management.the_user_is_signed_out_everywhere', defaultMessage: 'The user is signed out everywhere and mailed a link to choose a new password.' })}
            confirmLabel={t({ id: 'mastodon.admin.accounts.reset_password', defaultMessage: 'Reset password' })}
            onConfirm={async () => {
              onChanged(await userAccessAction(token, account.id, 'reset'))
              toast.success(t({ id: 'eunha.admin.user_management.password_reset', defaultMessage: 'Password reset.' }))
            }}
          >
            {t({ id: 'mastodon.admin.accounts.reset_password', defaultMessage: 'Reset password' })}
          </ConfirmButton>
          <ConfirmButton
            title={t({ id: 'eunha.admin.user_management.disable_two_factor_authentication', defaultMessage: 'Disable two-factor authentication?' })}
            description={t({ id: 'eunha.admin.user_management.the_user_can_then_sign_in', defaultMessage: 'The user can then sign in with their email address and password alone.' })}
            confirmLabel={t({ id: 'mastodon.admin.accounts.disable_two_factor_authentication', defaultMessage: 'Disable 2FA' })}
            onConfirm={async () => {
              onChanged(await disableTwoFactor(token, account.id))
              toast.success(t({ id: 'eunha.admin.user_management.two_factor_authentication_disabled', defaultMessage: 'Two-factor authentication disabled.' }))
            }}
          >
            {t({ id: 'mastodon.admin.accounts.disable_two_factor_authentication', defaultMessage: 'Disable 2FA' })}
          </ConfirmButton>
        </div>
      )}
      <Dialog open={emailOpen} onOpenChange={setEmailOpen}>
        <DialogContent>
          <form onSubmit={submitEmail} className="space-y-4">
            <DialogHeader>
              <DialogTitle>
                {t({ id: 'mastodon.admin.accounts.change_email.title', defaultMessage: 'Change email for {username}' }, { username: account.username })}
              </DialogTitle>
              <DialogDescription>
                {t({ id: 'eunha.admin.user_management.the_new_address_takes_over_once', defaultMessage: 'The new address takes over once its owner follows the confirmation link mailed to it.' })}
              </DialogDescription>
            </DialogHeader>
            <Input
              type="email"
              aria-label={t({ id: 'mastodon.devise.mailer.email_changed.title', defaultMessage: 'New email address' })}
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
            <DialogFooter>
              <DialogClose render={<Button type="button" variant="outline" />}>{t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}</DialogClose>
              <Button type="submit" disabled={busy || !email.trim()}>
                {t({ id: 'mastodon.admin.accounts.change_email.label', defaultMessage: 'Change email' })}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </section>
  )
}
