import { useEffect, useState, type FormEvent } from 'react'
import { Link } from 'react-router-dom'
import { ChevronRight } from 'lucide-react'
import { toast } from 'sonner'

import {
  disableEmailSubscriptions,
  getEmailSubscriptions,
  purgeEmailSubscriptions,
  setupEmailSubscriptions,
  updateEmailFooterText,
  type EmailSubscriptionStatus,
  type EmailSubscriptionsOverview,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminAvatar, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { CardsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import type { MessageDescriptor } from 'react-intl'
import { defineMessage } from 'react-intl'

// Mastodon's `email_subscriptions.*` badge labels.
const STATUS_LABEL: Record<EmailSubscriptionStatus, string> = {
  get active() { return t({ id: 'mastodon.email_subscriptions.active', defaultMessage: 'Active' }) },
  get disabled() { return t({ id: 'mastodon.email_subscriptions.disabled', defaultMessage: 'Disabled' }) },
  get no_access() { return t({ id: 'mastodon.email_subscriptions.no_access', defaultMessage: 'No access' }) },
  get inactive() { return t({ id: 'mastodon.email_subscriptions.inactive', defaultMessage: 'Inactive' }) },
}

export function EmailSubscriptionStatusBadge({ status }: { status: EmailSubscriptionStatus }) {
  return (
    <Badge variant={status === 'active' ? 'secondary' : 'destructive'}>
      {STATUS_LABEL[status]}
    </Badge>
  )
}

// `admin.email_subscriptions.setups.show.list`.
const IMPORTANT: MessageDescriptor[] = [
  defineMessage({ id: 'mastodon.admin.email_subscriptions.setups.show.list.1_permission_explanation', defaultMessage: 'When this feature is enabled, accounts with the designated permissions can add an email collection form to their profiles.' }),
  defineMessage({ id: 'mastodon.admin.email_subscriptions.setups.show.list.2_feature_explanation', defaultMessage: 'When visitors sign up on an account’s profile page and confirm their subscription, they’ll begin to receive email updates when the account creates new public posts.' }),
  defineMessage({ id: 'mastodon.admin.email_subscriptions.setups.show.list.3_privacy_policy_warning', defaultMessage: 'Server admins will have access to PII (email addresses) collected. As such, the privacy policy and Terms of Service for the server must be updated before using this feature.' }),
  defineMessage({ id: 'mastodon.admin.email_subscriptions.setups.show.list.4_cost_warning', defaultMessage: 'Emails may incur a fee depending on hosting setup. Discuss with your hosting provider before enabling, as this feature could drastically increase the amount of emails sent from your server.' }),
]

function Setup({
  token,
  onEnabled,
}: {
  token: string
  onEnabled: (o: EmailSubscriptionsOverview) => void
}) {
  const [volume, setVolume] = useState(false)
  const [privacy, setPrivacy] = useState(false)
  const [saving, setSaving] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      onEnabled(
        await setupEmailSubscriptions(token, {
          agreement_email_volume: volume,
          agreement_privacy_and_terms: privacy,
        }),
      )
      toast.success(t({ id: 'eunha.admin.email_subscriptions.email_newsletters_are_enabled', defaultMessage: 'Email newsletters are enabled.' }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'mastodon.admin.email_subscriptions.setups.show.important_information', defaultMessage: 'Important information' })}</h2>
      <ol className="text-muted-foreground list-decimal space-y-1 pl-5 text-sm">
        {IMPORTANT.map((item) => (
          <li key={item.id}>{t(item)}</li>
        ))}
      </ol>
      <Label className="text-sm font-normal">
        <Checkbox checked={volume} onCheckedChange={(v) => setVolume(v === true)} />{t({ id: 'eunha.admin.email_subscriptions.i_understand_this_can_greatly_increase', defaultMessage: 'I understand this can greatly increase the email this server sends' })}
      </Label>
      <Label className="text-sm font-normal">
        <Checkbox checked={privacy} onCheckedChange={(v) => setPrivacy(v === true)} />{t({ id: 'eunha.admin.email_subscriptions.i_have_updated_the_privacy_policy', defaultMessage: 'I have updated the privacy policy and terms of service' })}
      </Label>
      <Button type="submit" disabled={saving || !volume || !privacy}>
        {t({ id: 'mastodon.admin.email_subscriptions.accounts.show.enable_feature', defaultMessage: 'Enable feature' })}
      </Button>
    </form>
  )
}

function FooterText({
  token,
  initial,
  onSaved,
}: {
  token: string
  initial: string
  onSaved: (o: EmailSubscriptionsOverview) => void
}) {
  const [text, setText] = useState(initial)
  const [saving, setSaving] = useState(false)
  const save = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      onSaved(await updateEmailFooterText(token, text))
      toast.success(t({ id: 'eunha.common.saved', defaultMessage: 'Saved.' }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }
  return (
    <form onSubmit={save} className="space-y-2">
      <Label htmlFor="email-footer-text">{t({ id: 'mastodon.admin.email_subscriptions.additional_footer_texts.show.title', defaultMessage: 'Additional footer text' })}</Label>
      <Textarea
        id="email-footer-text"
        rows={4}
        value={text}
        onChange={(e) => setText(e.target.value)}
      />
      <p className="text-muted-foreground text-xs">
        {t({ id: 'eunha.admin.email_subscriptions.optional_text_that_appears_in_the', defaultMessage: 'Optional text that appears in the footer of newsletter emails only. The privacy policy, linked in the footer of every email, is edited with the instance’s configuration.' })}
      </p>
      <Button type="submit" size="sm" variant="secondary" disabled={saving}>
        {t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' })}
      </Button>
    </form>
  )
}

/**
 * Email newsletters: Mastodon's `/admin/email_subscriptions`. Until the
 * feature is enabled, the setup with its two agreements; then the roles that
 * may offer it, the accounts that have subscribers, the footer text, and the
 * switches to turn it off and erase every list.
 */
export default function EmailSubscriptions() {
  const token = getToken()
  const [overview, setOverview] = useState<EmailSubscriptionsOverview | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) return
    getEmailSubscriptions(token)
      .then(setOverview)
      .catch((e) => setError(errorMessage(e)))
  }, [token])

  let body = null
  if (overview && token) {
    if (!overview.enabled) {
      body = (
        <div className="space-y-3">
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.admin.email_subscriptions.this_feature_allows_specified_accounts_to', defaultMessage: 'This feature allows specified accounts to add a widget to their profiles, enabling visitors without an account to receive their posts via email.' })}
          </p>
          {overview.available ? (
            <Setup token={token} onEnabled={setOverview} />
          ) : (
            <p className="rounded-lg border p-3 text-sm">
              {t({ id: 'eunha.admin.email_subscriptions.whoever_runs_this_server_has_not', defaultMessage: 'Whoever runs this server has not enabled this feature for it.' })}
            </p>
          )}
        </div>
      )
    } else {
      body = (
        <div className="space-y-6">
          <section className="space-y-2">
            <h2 className="font-semibold">{t({ id: 'mastodon.admin.email_subscriptions.roles.title', defaultMessage: 'Roles' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.admin.email_subscriptions.accounts_with_the_following_roles_can', defaultMessage: 'Accounts with the following roles can enable this feature on their profiles.' })}
            </p>
            {overview.roles.length === 0 ? (
              <p className="text-muted-foreground text-sm">
                {t({ id: 'mastodon.admin.email_subscriptions.roles.empty.hint', defaultMessage: 'No one has permission to use this feature.' })}
              </p>
            ) : (
              <div className="divide-y rounded-lg border">
                {overview.roles.map((role) => (
                  <div key={role.id} className="flex items-center gap-2 p-2.5 text-sm">
                    <span className="flex-1 font-medium">{role.name}</span>
                    <span className="text-muted-foreground">
                      {role.accounts} {role.accounts === 1 ? 'account' : 'accounts'}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </section>

          <section className="space-y-2">
            <h2 className="font-semibold">{t({ id: 'mastodon.admin.email_subscriptions.accounts.title', defaultMessage: 'Mailing lists' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'mastodon.admin.email_subscriptions.accounts.lead', defaultMessage: 'Accounts who have enabled the feature and have subscribers will show below.' })}
            </p>
            {overview.accounts.length === 0 ? (
              <p className="text-muted-foreground text-sm">
                {t({ id: 'mastodon.admin.email_subscriptions.accounts.empty.hint', defaultMessage: 'No accounts have subscribers yet.' })}
              </p>
            ) : (
              <div className="divide-y rounded-lg border">
                {overview.accounts.map(({ account, status, subscribers, last_status_at }) => (
                  <Link
                    key={account.id}
                    to={`/admin/email_subscriptions/accounts/${account.id}`}
                    className="flex items-center gap-2 p-2.5 no-underline hover:bg-muted/50"
                  >
                    <AdminAvatar account={account} className="size-6" />
                    <span className="min-w-0 flex-1 truncate text-sm font-medium">
                      {account.display_name || account.username}
                    </span>
                    <EmailSubscriptionStatusBadge status={status} />
                    <span className="text-muted-foreground w-24 text-right text-xs">
                      {subscribers} {subscribers === 1 ? 'subscriber' : 'subscribers'}
                    </span>
                    <span className="text-muted-foreground hidden w-40 text-right text-xs sm:block">
                      {last_status_at ? formatDate(last_status_at) : '–'}
                    </span>
                    <ChevronRight className="text-muted-foreground size-4" />
                  </Link>
                ))}
              </div>
            )}
          </section>

          <section className="space-y-2">
            <h2 className="font-semibold">{t({ id: 'mastodon.admin.email_subscriptions.compliance_settings.title', defaultMessage: 'Compliance settings' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'mastodon.admin.email_subscriptions.compliance_settings.lead', defaultMessage: 'Email newsletters may be considered marketing emails, depending on the jurisdictions where you operate.' })}
            </p>
            <FooterText
              token={token}
              initial={overview.email_footer_text}
              onSaved={setOverview}
            />
          </section>

          <section className="border-destructive/40 space-y-3 rounded-lg border p-4">
            <h2 className="text-destructive font-semibold">{t({ id: 'mastodon.admin.email_subscriptions.danger_zone.title', defaultMessage: 'Danger zone' })}</h2>
            <div className="flex flex-wrap items-center gap-2">
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium">{t({ id: 'mastodon.admin.email_subscriptions.accounts.show.disable_feature', defaultMessage: 'Disable feature' })}</div>
                <div className="text-muted-foreground text-xs">
                  {t({ id: 'mastodon.admin.email_subscriptions.danger_zone.disable_feature.hint', defaultMessage: 'Turn off feature for all accounts' })}
                </div>
              </div>
              <ConfirmButton
                title={t({ id: 'eunha.admin.email_subscriptions.disable_email_newsletters', defaultMessage: 'Disable email newsletters?' })}
                description={t({ id: 'eunha.admin.email_subscriptions.no_account_will_be_able_to', defaultMessage: 'No account will be able to offer email subscriptions, and no more newsletters will be sent. The lists are kept.' })}
                confirmLabel={t({ id: 'mastodon.admin.email_subscriptions.danger_zone.disable_feature.action', defaultMessage: 'Disable' })}
                onConfirm={async () => {
                  setOverview(await disableEmailSubscriptions(token))
                  toast.success(t({ id: 'eunha.admin.email_subscriptions.email_subscriptions_have_been_successfully_disabled', defaultMessage: 'Email subscriptions have been successfully disabled.' }))
                }}
              >
                {t({ id: 'mastodon.admin.email_subscriptions.danger_zone.disable_feature.action', defaultMessage: 'Disable' })}
              </ConfirmButton>
            </div>
            {overview.accounts.length > 0 && (
              <div className="flex flex-wrap items-center gap-2">
                <div className="min-w-0 flex-1">
                  <div className="text-sm font-medium">{t({ id: 'mastodon.admin.email_subscriptions.danger_zone.erase_all_data.title', defaultMessage: 'Erase all data' })}</div>
                  <div className="text-muted-foreground text-xs">
                    {t({ id: 'mastodon.admin.email_subscriptions.danger_zone.erase_all_data.hint', defaultMessage: 'Permanently deletes all emails in all mailing lists' })}
                  </div>
                </div>
                <ConfirmButton
                  title={t({ id: 'eunha.admin.email_subscriptions.erase_every_mailing_list', defaultMessage: 'Erase every mailing list?' })}
                  description={t({ id: 'eunha.admin.email_subscriptions.every_subscriber_of_every_account_is', defaultMessage: 'Every subscriber of every account is deleted. This cannot be undone.' })}
                  confirmLabel={t({ id: 'mastodon.admin.email_subscriptions.danger_zone.erase_all_data.action', defaultMessage: 'Erase data' })}
                  onConfirm={async () => {
                    setOverview(await purgeEmailSubscriptions(token))
                    toast.success(t({ id: 'eunha.admin.email_subscriptions.all_email_subscriptions_data_has_been', defaultMessage: 'All email subscriptions data has been erased.' }))
                  }}
                >
                  {t({ id: 'mastodon.admin.email_subscriptions.danger_zone.erase_all_data.action', defaultMessage: 'Erase data' })}
                </ConfirmButton>
              </div>
            )}
          </section>
        </div>
      )
    }
  }

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.email_subscriptions.index.title', defaultMessage: 'Email newsletters' })} permission="manage_settings">
      <p className="text-muted-foreground mb-4 text-sm">
        {t({ id: 'mastodon.admin.email_subscriptions.index.lead', defaultMessage: 'Allow visitors to receive posts via email from dedicated accounts on this server.' })}
      </p>
      <AdminError error={error} />
      {!overview && !error && <CardsSkeleton />}
      {body}
    </AdminLayout>
  )
}
