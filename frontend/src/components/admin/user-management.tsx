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
    listAssignableRoles(token).then(setRoles).catch(() => {})
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

  const roleItems: Record<string, string> = { [NO_ROLE]: 'No role' }
  for (const r of roles) roleItems[r.id] = r.name

  const submitEmail = async (e: FormEvent) => {
    e.preventDefault()
    await run(() => changeUserEmail(token, account.id, email.trim()), 'Confirmation link sent to the new address.')
    setEmailOpen(false)
  }

  return (
    <section className="space-y-2">
      <h2 className="text-sm font-semibold">Sign-in and role</h2>
      {manageRoles && (
        <div className="flex flex-wrap items-center gap-2">
          <ChoiceSelect
            label="Role"
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
                'Role changed.',
              )
            }
          >
            Change role
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
            Change email
          </Button>
          {!account.confirmed && (
            <>
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                onClick={() =>
                  void run(() => userAccessAction(token, account.id, 'confirmation'), 'Confirmed.')
                }
              >
                Confirm
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                onClick={() =>
                  void run(
                    () => userAccessAction(token, account.id, 'confirmation/resend'),
                    'Confirmation link successfully sent!',
                  )
                }
              >
                Resend confirmation link
              </Button>
            </>
          )}
          <ConfirmButton
            title="Reset the password?"
            description="The user is signed out everywhere and mailed a link to choose a new password."
            confirmLabel="Reset password"
            onConfirm={async () => {
              onChanged(await userAccessAction(token, account.id, 'reset'))
              toast.success('Password reset.')
            }}
          >
            Reset password
          </ConfirmButton>
          <ConfirmButton
            title="Disable two-factor authentication?"
            description="The user can then sign in with their email address and password alone."
            confirmLabel="Disable 2FA"
            onConfirm={async () => {
              onChanged(await disableTwoFactor(token, account.id))
              toast.success('Two-factor authentication disabled.')
            }}
          >
            Disable 2FA
          </ConfirmButton>
        </div>
      )}
      <Dialog open={emailOpen} onOpenChange={setEmailOpen}>
        <DialogContent>
          <form onSubmit={submitEmail} className="space-y-4">
            <DialogHeader>
              <DialogTitle>Change email for {account.username}</DialogTitle>
              <DialogDescription>
                The new address takes over once its owner follows the confirmation link mailed to
                it.
              </DialogDescription>
            </DialogHeader>
            <Input
              type="email"
              aria-label="New email address"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
            <DialogFooter>
              <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
              <Button type="submit" disabled={busy || !email.trim()}>
                Change email
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </section>
  )
}
