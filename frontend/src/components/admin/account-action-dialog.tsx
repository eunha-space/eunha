import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  accountAction,
  publicAccount,
  type AccountActionType,
  type AdminAccount,
} from '../../admin-api.ts'
import { errorMessage } from '@/lib/utils.ts'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog.tsx'
import { Label } from '@/components/ui/label.tsx'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

// Mastodon's `Admin::AccountAction::TYPES`, labelled and explained as its
// admin form does (`account_action_type_label` and the type hints).
const TYPES: Record<AccountActionType, { label: string; hint: string }> = {
  none: {
    label: 'Warning',
    hint: 'Send a warning to the user, without any other action.',
  },
  disable: {
    label: 'Freeze',
    hint: 'Keep the user from using their account, without deleting or hiding what they have posted.',
  },
  sensitive: {
    label: 'Force-sensitive',
    hint: "Mark all of this user's media as sensitive.",
  },
  silence: {
    label: 'Limit',
    hint: 'Keep their posts from anyone who does not follow them, and stop them from posting publicly. Closes every report against this account.',
  },
  suspend: {
    label: 'Suspend',
    hint: 'Stop all interaction with this account and delete what it has posted. Can be undone within 30 days. Closes every report against this account.',
  },
}

/** `Admin::AccountAction.types_for_account`: warnings and freezing need a local user. */
function typesFor(account: AdminAccount): AccountActionType[] {
  const all = Object.keys(TYPES) as AccountActionType[]
  return account.domain === null ? all : all.filter((t) => t !== 'none' && t !== 'disable')
}

/** `Admin::AccountAction.disabled_types_for_account`. */
function disabledFor(account: AdminAccount): AccountActionType[] {
  if (account.suspended) return ['silence', 'suspend']
  if (account.silenced) return ['silence']
  return []
}

/**
 * Take action against an account: Mastodon's admin "Perform moderation action"
 * form, on `POST /api/v1/admin/accounts/:id/action`.
 *
 * From a report, `reportId` ties the action to it: a warning resolves that
 * report alone, and anything stronger resolves every open report on the
 * account, as upstream does.
 *
 * Mastodon's form also offers warning presets. There is no API that lists
 * them, so there is nothing to pick from here; a typed warning still goes out.
 */
export function AccountActionDialog({
  account,
  reportId,
  open,
  onOpenChange,
  onDone,
  token,
  initialType,
}: {
  account: AdminAccount
  reportId?: string
  open: boolean
  onOpenChange: (open: boolean) => void
  /** After the action succeeds; callers reload what it changed. */
  onDone: () => void
  token: string
  initialType?: AccountActionType
}) {
  const local = account.domain === null
  const types = typesFor(account)
  const disabled = disabledFor(account)
  const firstEnabled = types.find((t) => !disabled.includes(t)) ?? types[0]
  const [type, setType] = useState<AccountActionType>(firstEnabled)
  const [text, setText] = useState('')
  const [notify, setNotify] = useState(true)
  const [sending, setSending] = useState(false)

  useEffect(() => {
    if (!open) return
    setType(initialType && types.includes(initialType) && !disabled.includes(initialType)
      ? initialType
      : firstEnabled)
    setText('')
    setNotify(true)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const acct = publicAccount(account).acct

  const submit = async () => {
    if (sending) return
    setSending(true)
    try {
      await accountAction(token, account.id, {
        type,
        report_id: reportId,
        // Only a local user can be written to: a remote one has no inbox here
        // for the warning or the email.
        text: local && text.trim() ? text.trim() : undefined,
        send_email_notification: local ? notify : undefined,
      })
      toast.success(`${TYPES[type].label}: done for @${acct}.`)
      onOpenChange(false)
      onDone()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSending(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Moderate @{acct}</DialogTitle>
          <DialogDescription>
            {account.suspended
              ? 'This account is already suspended.'
              : account.silenced
                ? 'This account is already limited.'
                : 'Choose what happens to this account.'}
          </DialogDescription>
        </DialogHeader>

        <RadioGroup
          value={type}
          onValueChange={(v) => setType(v as AccountActionType)}
          aria-label="Action"
        >
          {types.map((t) => (
            <Label
              key={t}
              className="items-start gap-3 rounded-lg border p-3 font-normal has-data-checked:border-primary"
            >
              <RadioGroupItem value={t} disabled={disabled.includes(t)} className="mt-0.5" />
              <span className="space-y-0.5">
                <span className="block text-sm font-medium">{TYPES[t].label}</span>
                <span className="text-muted-foreground block text-xs">{TYPES[t].hint}</span>
              </span>
            </Label>
          ))}
        </RadioGroup>

        {local && (
          <div className="space-y-3">
            <Label className="text-sm font-normal">
              <Checkbox checked={notify} onCheckedChange={(v) => setNotify(v === true)} />
              Notify the user by email
            </Label>
            <div className="space-y-1">
              <Label htmlFor="action-text">Custom warning</Label>
              <Textarea
                id="action-text"
                value={text}
                rows={3}
                className="resize-y"
                placeholder="What should the user be told?"
                onChange={(e) => setText(e.target.value)}
              />
            </div>
          </div>
        )}

        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={sending} />}>
            Cancel
          </DialogClose>
          <Button
            variant={type === 'none' ? 'default' : 'destructive'}
            disabled={sending || disabled.includes(type)}
            onClick={() => void submit()}
          >
            {sending ? 'Working…' : TYPES[type].label}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
