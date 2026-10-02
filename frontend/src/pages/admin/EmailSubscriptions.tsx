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
import { ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

// Mastodon's `email_subscriptions.*` badge labels.
const STATUS_LABEL: Record<EmailSubscriptionStatus, string> = {
  active: 'Active',
  disabled: 'Disabled',
  no_access: 'No access',
  inactive: 'Inactive',
}

export function EmailSubscriptionStatusBadge({ status }: { status: EmailSubscriptionStatus }) {
  return (
    <Badge variant={status === 'active' ? 'secondary' : 'destructive'}>
      {STATUS_LABEL[status]}
    </Badge>
  )
}

// `admin.email_subscriptions.setups.show.list`.
const IMPORTANT = [
  'When this feature is enabled, accounts with the designated permissions can add an email collection form to their profiles.',
  'When visitors sign up on an account’s profile page and confirm their subscription, they’ll begin to receive email updates when the account creates new public posts.',
  'Server admins will have access to PII (email addresses) collected. As such, the privacy policy and Terms of Service for the server must be updated before using this feature.',
  'Emails may incur a fee depending on hosting setup. Discuss with your hosting provider before enabling, as this feature could drastically increase the amount of emails sent from your server.',
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
      toast.success('Email newsletters are enabled.')
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">Important information</h2>
      <ol className="text-muted-foreground list-decimal space-y-1 pl-5 text-sm">
        {IMPORTANT.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ol>
      <Label className="text-sm font-normal">
        <Checkbox checked={volume} onCheckedChange={(v) => setVolume(v === true)} />I understand
        this can greatly increase the email this server sends
      </Label>
      <Label className="text-sm font-normal">
        <Checkbox checked={privacy} onCheckedChange={(v) => setPrivacy(v === true)} />I have
        updated the privacy policy and terms of service
      </Label>
      <Button type="submit" disabled={saving || !volume || !privacy}>
        Enable feature
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
      toast.success('Saved.')
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }
  return (
    <form onSubmit={save} className="space-y-2">
      <Label htmlFor="email-footer-text">Additional footer text</Label>
      <Textarea
        id="email-footer-text"
        rows={4}
        value={text}
        onChange={(e) => setText(e.target.value)}
      />
      <p className="text-muted-foreground text-xs">
        Optional text that appears in the footer of newsletter emails only. The privacy policy,
        linked in the footer of every email, is edited with the instance’s configuration.
      </p>
      <Button type="submit" size="sm" variant="secondary" disabled={saving}>
        Save changes
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
            This feature allows specified accounts to add a widget to their profiles, enabling
            visitors without an account to receive their posts via email.
          </p>
          {overview.available ? (
            <Setup token={token} onEnabled={setOverview} />
          ) : (
            <p className="rounded-lg border p-3 text-sm">
              Whoever runs this server has not enabled this feature for it.
            </p>
          )}
        </div>
      )
    } else {
      body = (
        <div className="space-y-6">
          <section className="space-y-2">
            <h2 className="font-semibold">Roles</h2>
            <p className="text-muted-foreground text-sm">
              Accounts with the following roles can enable this feature on their profiles.
            </p>
            {overview.roles.length === 0 ? (
              <p className="text-muted-foreground text-sm">
                No one has permission to use this feature.
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
            <h2 className="font-semibold">Mailing lists</h2>
            <p className="text-muted-foreground text-sm">
              Accounts who have enabled the feature and have subscribers will show below.
            </p>
            {overview.accounts.length === 0 ? (
              <p className="text-muted-foreground text-sm">
                No accounts have subscribers yet.
              </p>
            ) : (
              <div className="divide-y rounded-lg border">
                {overview.accounts.map(({ account, status, subscribers, last_status_at }) => (
                  <Link
                    key={account.id}
                    to={`/admin/email_subscriptions/accounts/${account.id}`}
                    className="flex items-center gap-2 p-2.5 no-underline hover:bg-muted/50"
                  >
                    <Avatar className="size-6">
                      <AvatarImage src={account.avatar} alt="" />
                      <AvatarFallback>
                        {(account.display_name || account.username).slice(0, 1).toUpperCase()}
                      </AvatarFallback>
                    </Avatar>
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
            <h2 className="font-semibold">Compliance settings</h2>
            <p className="text-muted-foreground text-sm">
              Email newsletters may be considered marketing emails, depending on the
              jurisdictions where you operate.
            </p>
            <FooterText
              token={token}
              initial={overview.email_footer_text}
              onSaved={setOverview}
            />
          </section>

          <section className="border-destructive/40 space-y-3 rounded-lg border p-4">
            <h2 className="text-destructive font-semibold">Danger zone</h2>
            <div className="flex flex-wrap items-center gap-2">
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium">Disable feature</div>
                <div className="text-muted-foreground text-xs">
                  Turn off feature for all accounts
                </div>
              </div>
              <ConfirmButton
                title="Disable email newsletters?"
                description="No account will be able to offer email subscriptions, and no more newsletters will be sent. The lists are kept."
                confirmLabel="Disable"
                onConfirm={async () => {
                  setOverview(await disableEmailSubscriptions(token))
                  toast.success('Email subscriptions have been successfully disabled.')
                }}
              >
                Disable
              </ConfirmButton>
            </div>
            {overview.accounts.length > 0 && (
              <div className="flex flex-wrap items-center gap-2">
                <div className="min-w-0 flex-1">
                  <div className="text-sm font-medium">Erase all data</div>
                  <div className="text-muted-foreground text-xs">
                    Permanently deletes all emails in all mailing lists
                  </div>
                </div>
                <ConfirmButton
                  title="Erase every mailing list?"
                  description="Every subscriber of every account is deleted. This cannot be undone."
                  confirmLabel="Erase data"
                  onConfirm={async () => {
                    setOverview(await purgeEmailSubscriptions(token))
                    toast.success('All email subscriptions data has been erased.')
                  }}
                >
                  Erase data
                </ConfirmButton>
              </div>
            )}
          </section>
        </div>
      )
    }
  }

  return (
    <AdminLayout title="Email newsletters" permission="manage_settings">
      <p className="text-muted-foreground mb-4 text-sm">
        Allow visitors to receive posts via email from dedicated accounts on this server.
      </p>
      <AdminError error={error} />
      {!overview && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {body}
    </AdminLayout>
  )
}
