import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  accountAction,
  listWarningPresets,
  publicAccount,
  reportModerationAction,
  type AccountActionType,
  type AdminAccount,
  type AdminReport,
  type ModerationActionType,
  type WarningPreset,
} from '../../admin-api.ts'
import { ChoiceSelect } from '@/components/admin/admin-common.tsx'
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
import { t } from '@/i18n.tsx'

/** An account action, or, from a report, an action on what it cites. */
export type ActionType = AccountActionType | ModerationActionType

// Mastodon's `Admin::AccountAction::TYPES` and, from a report,
// `Admin::ModerationAction::TYPES`, labelled and explained as its admin form
// does (`account_actions_combined_types_collection` and the type hints).
const TYPES: Record<ActionType, { label: string; hint: string }> = {
  none: {
    get label() { return t({ id: 'mastodon.admin.instances.back_to_warning', defaultMessage: 'Warning' }) },
    get hint() { return t({ id: 'eunha.admin.account_action_dialog.send_a_warning_to_the_user', defaultMessage: 'Send a warning to the user, without any other action.' }) },
  },
  disable: {
    get label() { return t({ id: 'mastodon.admin.accounts.disable', defaultMessage: 'Freeze' }) },
    get hint() { return t({ id: 'eunha.admin.account_action_dialog.keep_the_user_from_using_their', defaultMessage: 'Keep the user from using their account, without deleting or hiding what they have posted.' }) },
  },
  sensitive: {
    label: 'Force-sensitive',
    get hint() { return t({ id: 'eunha.admin.account_action_dialog.mark_all_of_this_user_s', defaultMessage: 'Mark all of this user\'\'s media as sensitive.' }) },
  },
  silence: {
    get label() { return t({ id: 'mastodon.admin.accounts.silence', defaultMessage: 'Limit' }) },
    get hint() { return t({ id: 'eunha.admin.account_action_dialog.keep_their_posts_from_anyone_who', defaultMessage: 'Keep their posts from anyone who does not follow them, and stop them from posting publicly. Closes every report against this account.' }) },
  },
  suspend: {
    get label() { return t({ id: 'mastodon.admin.accounts.perform_full_suspension', defaultMessage: 'Suspend' }) },
    get hint() { return t({ id: 'eunha.admin.account_action_dialog.stop_all_interaction_with_this_account', defaultMessage: 'Stop all interaction with this account and delete what it has posted. Can be undone within 30 days. Closes every report against this account.' }) },
  },
  mark_as_sensitive: {
    get label() { return t({ id: 'mastodon.admin.account_actions.types.mark_as_sensitive', defaultMessage: 'Mark posts as sensitive' }) },
    get hint() { return t({ id: 'mastodon.admin.reports.actions.mark_as_sensitive_description_html', defaultMessage: 'The media in the reported posts is hidden behind a warning, this report is resolved, and a strike is recorded.' }) },
  },
  delete: {
    get label() { return t({ id: 'mastodon.admin.account_actions.types.delete', defaultMessage: 'Delete posts' }) },
    get hint() { return t({ id: 'mastodon.admin.reports.actions.delete_description_html', defaultMessage: 'The reported posts and collections are deleted, this report is resolved, and a strike is recorded. This cannot be undone.' }) },
  },
}

const NO_PRESET = 'none'

/**
 * `Admin::AccountAction.types_for_account`, where warnings and freezing need
 * a local user, and from a report the actions on what it cites.
 */
function typesFor(account: AdminAccount, report?: AdminReport): ActionType[] {
  const all: AccountActionType[] = ['none', 'disable', 'sensitive', 'silence', 'suspend']
  const types: ActionType[] =
    account.domain === null ? all : all.filter((t) => t !== 'none' && t !== 'disable')
  return report ? [...types, 'mark_as_sensitive', 'delete'] : types
}

/**
 * `Admin::AccountAction.disabled_types_for_account`; and the actions on a
 * report's posts once it is resolved, or when it cites none.
 */
function disabledFor(account: AdminAccount, report?: AdminReport): ActionType[] {
  const disabled: ActionType[] = account.suspended
    ? ['silence', 'suspend']
    : account.silenced
      ? ['silence']
      : []
  if (report && (report.action_taken || report.statuses.length === 0)) {
    disabled.push('mark_as_sensitive', 'delete')
  }
  return disabled
}

/**
 * Take action against an account: Mastodon's admin "Perform moderation action"
 * form, on `POST /api/v1/admin/accounts/:id/action`.
 *
 * From a report, `report` ties the action to it: a warning resolves that
 * report alone, and anything stronger resolves every open report on the
 * account, as upstream does. The form then also offers deleting or marking
 * sensitive what the report cites, on `POST /api/v1/admin/reports/:id/actions`,
 * and whether the strike cites the reported posts and, for a report of broken
 * rules, whether its email names them.
 *
 * A warning preset, when one is picked, leads the warning, and the custom text
 * follows it, as upstream's `text_for_warning` joins them. As upstream's form
 * does, turning the email off hides what it would say and leaves the posts
 * out of the strike.
 */
export function AccountActionDialog({
  account,
  report,
  open,
  onOpenChange,
  onDone,
  token,
  initialType,
}: {
  account: AdminAccount
  report?: AdminReport
  open: boolean
  onOpenChange: (open: boolean) => void
  /** After the action succeeds; callers reload what it changed. */
  onDone: () => void
  token: string
  initialType?: ActionType
}) {
  const local = account.domain === null
  const types = typesFor(account, report)
  const disabled = disabledFor(account, report)
  const firstEnabled = types.find((t) => !disabled.includes(t)) ?? types[0]
  const [type, setType] = useState<ActionType>(firstEnabled)
  const [text, setText] = useState('')
  const [notify, setNotify] = useState(true)
  const [includeStatuses, setIncludeStatuses] = useState(true)
  const [includeRules, setIncludeRules] = useState(true)
  const [sending, setSending] = useState(false)
  const [presets, setPresets] = useState<WarningPreset[]>([])
  const [presetId, setPresetId] = useState<string>(NO_PRESET)
  // The rules a report of broken rules cites, which the email can name.
  const citedRules = report?.category === 'violation' ? report.rules : []

  useEffect(() => {
    if (!open || !local) return
    listWarningPresets(token)
      .then(setPresets)
      .catch(() => setPresets([]))
  }, [open, local, token])

  useEffect(() => {
    if (!open) return
    setPresetId(NO_PRESET)
    setType(initialType && types.includes(initialType) && !disabled.includes(initialType)
      ? initialType
      : firstEnabled)
    setText('')
    setNotify(true)
    setIncludeStatuses(true)
    setIncludeRules(true)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // The presets for the action picked (`admin.tsx`'s `presets_dropdown`).
  const offered = presets.filter(
    (p) => p.action_category === 'all_actions' || p.action_category === type,
  )
  useEffect(() => {
    if (presetId !== NO_PRESET && !offered.some((p) => p.id === presetId)) {
      setPresetId(NO_PRESET)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [type])

  const acct = publicAccount(account).acct
  // Only a local user can be written to: a remote one has no inbox here for
  // the warning or the email.
  const writing = local && notify

  const submit = async () => {
    if (sending) return
    setSending(true)
    try {
      const options = {
        warning_preset_id: writing && presetId !== NO_PRESET ? presetId : undefined,
        text: writing && text.trim() ? text.trim() : undefined,
        send_email_notification: local ? notify : undefined,
        include_statuses: report && local ? writing && includeStatuses : undefined,
        include_rule_violations:
          report && local && citedRules.length > 0 ? includeRules : undefined,
      }
      if (report && (type === 'delete' || type === 'mark_as_sensitive')) {
        await reportModerationAction(token, report.id, type, options)
      } else {
        await accountAction(token, account.id, {
          type: type as AccountActionType,
          report_id: report?.id,
          ...options,
        })
      }
      toast.success(t({ id: 'eunha.admin.account_action_dialog.done_for', defaultMessage: '{label}: done for @{acct}.' }, { label: TYPES[type].label, acct }))
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
          <DialogTitle>
            {t({ id: 'eunha.admin.account_action_dialog.title', defaultMessage: 'Moderate @{acct}' }, { acct })}
          </DialogTitle>
          <DialogDescription>
            {account.suspended
              ? t({ id: 'eunha.admin.account_action_dialog.this_account_is_already_suspended', defaultMessage: 'This account is already suspended.' })
              : account.silenced
                ? t({ id: 'eunha.admin.account_action_dialog.this_account_is_already_limited', defaultMessage: 'This account is already limited.' })
                : t({ id: 'eunha.admin.account_action_dialog.choose_what_happens_to_this_account', defaultMessage: 'Choose what happens to this account.' })}
          </DialogDescription>
        </DialogHeader>

        <RadioGroup
          value={type}
          onValueChange={(v) => setType(v as ActionType)}
          aria-label={t({ id: 'mastodon.simple_form.labels.admin_account_action.type', defaultMessage: 'Action' })}
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
              {t({ id: 'eunha.admin.account_action_dialog.notify_the_user_by_email', defaultMessage: 'Notify the user by email' })}
            </Label>
            {notify && offered.length > 0 && (
              <div className="space-y-1">
                <Label>{t({ id: 'eunha.admin.account_action_dialog.use_a_warning_preset', defaultMessage: 'Use a warning preset' })}</Label>
                <ChoiceSelect
                  label={t({ id: 'eunha.admin.account_action_dialog.warning_preset', defaultMessage: 'Warning preset' })}
                  value={presetId}
                  items={{
                    [NO_PRESET]: t({ id: 'mastodon.admin.reports.comment.none', defaultMessage: 'None' }),
                    ...Object.fromEntries(offered.map((p) => [p.id, p.title || p.text.slice(0, 40)])),
                  }}
                  onChange={setPresetId}
                />
                {presetId !== NO_PRESET && (
                  <p className="text-muted-foreground text-xs whitespace-pre-wrap">
                    {offered.find((p) => p.id === presetId)?.text}
                  </p>
                )}
              </div>
            )}
            {notify && (
              <div className="space-y-1">
                <Label htmlFor="action-text">{t({ id: 'eunha.admin.account_action_dialog.custom_warning', defaultMessage: 'Custom warning' })}</Label>
                <Textarea
                  id="action-text"
                  value={text}
                  rows={3}
                  className="resize-y"
                  placeholder={t({ id: 'eunha.admin.account_action_dialog.what_should_the_user_be_told', defaultMessage: 'What should the user be told?' })}
                  onChange={(e) => setText(e.target.value)}
                />
              </div>
            )}
            {notify && report && (
              <Label className="items-start text-sm font-normal">
                <Checkbox
                  checked={includeStatuses}
                  onCheckedChange={(v) => setIncludeStatuses(v === true)}
                  className="mt-0.5"
                />
                <span className="space-y-0.5">
                  <span className="block">{t({ id: 'mastodon.simple_form.labels.admin_account_action.include_statuses', defaultMessage: 'Include the reported posts' })}</span>
                  <span className="text-muted-foreground block text-xs">{t({ id: 'mastodon.simple_form.hints.admin_account_action.include_statuses', defaultMessage: 'The user will see which posts led to this action or warning.' })}</span>
                </span>
              </Label>
            )}
            {notify && report && citedRules.length > 0 && (
              <div className="space-y-1">
                <Label className="items-start text-sm font-normal">
                  <Checkbox
                    checked={includeRules}
                    onCheckedChange={(v) => setIncludeRules(v === true)}
                    className="mt-0.5"
                  />
                  <span className="space-y-0.5">
                    <span className="block">{t({ id: 'mastodon.simple_form.labels.admin_account_action.include_rule_violatins', defaultMessage: 'Include the reported rule violations' })}</span>
                    <span className="text-muted-foreground block text-xs">{t({ id: 'mastodon.simple_form.hints.admin_account_action.include_rule_violations', defaultMessage: 'The email will say which rules led to this action.' })}</span>
                  </span>
                </Label>
                <ul className="text-muted-foreground list-disc ps-10 text-xs">
                  {citedRules.map((rule) => (
                    <li key={rule.id}>{rule.text}</li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        )}

        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={sending} />}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
          </DialogClose>
          <Button
            variant={type === 'none' || type === 'mark_as_sensitive' ? 'default' : 'destructive'}
            disabled={sending || disabled.includes(type)}
            onClick={() => void submit()}
          >
            {sending ? t({ id: 'eunha.common.working', defaultMessage: 'Working…' }) : TYPES[type].label}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
