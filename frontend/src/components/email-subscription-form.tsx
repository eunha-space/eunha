import { useId, useState, type FormEvent } from 'react'
import { Link } from 'react-router-dom'

import { SubscribeError, subscribeByEmail } from '../eunha-api.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

/**
 * The form Mastodon 4.7 puts on the profile of an account that offers email
 * subscriptions, for visitors who are not signed in
 * (`account_header/subscription_form`). An address already subscribed is
 * told the same as a new one, so the form says nothing about who subscribes.
 */
export function EmailSubscriptionForm({ accountId, name }: { accountId: string; name: string }) {
  const inputId = useId()
  const [email, setEmail] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const [submitted, setSubmitted] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    if (email.length === 0) return
    setSubmitting(true)
    try {
      await subscribeByEmail(accountId, email)
      setSubmitted(true)
    } catch (err) {
      const first = err instanceof SubscribeError ? err.details?.email : undefined
      if (first?.some((k) => k.error === 'ERR_TAKEN')) {
        setSubmitted(true)
      } else if (first?.[0]?.error === 'ERR_BLOCKED') {
        setError(t({ id: 'email_subscriptions.validation.email.blocked', defaultMessage: 'Blocked email provider' }))
      } else {
        setError(t({ id: 'email_subscriptions.validation.email.invalid', defaultMessage: 'Invalid email address' }))
      }
    } finally {
      setSubmitting(false)
    }
  }

  if (submitted) {
    return (
      <div className="mb-4 rounded-lg border p-4 text-center">
        <h2 className="font-semibold">{t({ id: 'email_subscriptions.submitted.title', defaultMessage: 'One more step' })}</h2>
        <p className="text-muted-foreground text-sm">
          {t({ id: 'email_subscriptions.submitted.lead', defaultMessage: 'Check your inbox for an email to finish signing up for email updates.' })}
        </p>
      </div>
    )
  }

  return (
    <form onSubmit={submit} noValidate className="mb-4 space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">
        {t({ id: 'email_subscriptions.form.title', defaultMessage: 'Sign up for email updates from {name}' }, { name })}
      </h2>
      <div className="flex gap-2">
        <div className="min-w-0 flex-1 space-y-1">
          <Label htmlFor={inputId} className="sr-only">
            {t({ id: 'email_subscriptions.email', defaultMessage: 'Email' })}
          </Label>
          <Input
            id={inputId}
            type="email"
            placeholder={t({ id: 'email_subscriptions.email', defaultMessage: 'Email' })}
            value={email}
            aria-invalid={error ? true : undefined}
            onChange={(e) => {
              setEmail(e.target.value)
              setError(null)
            }}
          />
          {error && <p className="text-destructive text-xs">{error}</p>}
        </div>
        <Button type="submit" disabled={submitting}>
          {t({ id: 'email_subscriptions.form.action', defaultMessage: 'Subscribe' })}
        </Button>
      </div>
      <p className="text-muted-foreground text-xs">
        <FormattedMessage
          id="email_subscriptions.form.bottom"
          defaultMessage="Get posts in your inbox without creating an account. Unsubscribe at any time. For more information, refer to the <a>Privacy Policy</a>."
          values={{ a: (chunks) => <Link to="/about">{chunks}</Link> }}
        />
      </p>
    </form>
  )
}
