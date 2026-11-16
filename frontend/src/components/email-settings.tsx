import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  changeAccountEmail,
  getAccountEmail,
  SecurityError,
  type AccountEmail,
} from '../security-api.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/**
 * The email half of Mastodon's account settings form: a new address, with the
 * current password, waits for the link mailed to it before it takes effect.
 */
export function EmailSettings({ token }: { token: string }) {
  const [current, setCurrent] = useState<AccountEmail | null>(null)
  const [failed, setFailed] = useState(false)
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    getAccountEmail(token)
      .then((e) => {
        setCurrent(e)
        setEmail(e.email)
      })
      .catch(() => setFailed(true))
  }, [token])

  if (!current) return failed ? null : <SectionSkeleton rows={2} />

  const save = async () => {
    setSaving(true)
    try {
      const updated = await changeAccountEmail(token, email, password)
      setCurrent(updated)
      setPassword('')
      if (updated.unconfirmed_email) {
        // Devise's `update_needs_confirmation`.
        toast.success(
          t({ id: 'eunha.email_settings.we_need_to_verify_your_new', defaultMessage: 'We need to verify your new email address. Please check your email and follow the confirm link to confirm your new email address.' }),
        )
      }
    } catch (e) {
      toast.error(e instanceof SecurityError ? e.message : t({ id: 'eunha.email_settings.could_not_change_your_email_address', defaultMessage: 'Could not change your email address' }))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'mastodon.admin.email_subscriptions.accounts.show.email', defaultMessage: 'Email address' })}</h2>
      {current.unconfirmed_email && (
        <p className="text-muted-foreground text-sm">
          {t(
            {
              id: 'eunha.email_settings.waiting_for_confirmation',
              defaultMessage: 'Waiting for you to confirm {unconfirmed}. Until then, mail goes to {email}.',
            },
            { unconfirmed: current.unconfirmed_email, email: current.email },
          )}
        </p>
      )}
      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault()
          void save()
        }}
      >
        <div className="space-y-1">
          <Label htmlFor="account-email">{t({ id: 'mastodon.admin.email_subscriptions.accounts.show.email', defaultMessage: 'Email address' })}</Label>
          <Input
            id="account-email"
            type="email"
            autoComplete="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor="account-email-password">{t({ id: 'mastodon.simple_form.labels.defaults.current_password', defaultMessage: 'Current password' })}</Label>
          <Input
            id="account-email-password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </div>
        <Button type="submit" size="sm" disabled={saving}>
          {t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' })}
        </Button>
      </form>
    </section>
  )
}
