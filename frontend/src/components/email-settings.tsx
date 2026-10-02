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

/**
 * The email half of Mastodon's account settings form: a new address, with the
 * current password, waits for the link mailed to it before it takes effect.
 */
export function EmailSettings({ token }: { token: string }) {
  const [current, setCurrent] = useState<AccountEmail | null>(null)
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    getAccountEmail(token)
      .then((e) => {
        setCurrent(e)
        setEmail(e.email)
      })
      .catch(() => {})
  }, [token])

  if (!current) return null

  const save = async () => {
    setSaving(true)
    try {
      const updated = await changeAccountEmail(token, email, password)
      setCurrent(updated)
      setPassword('')
      if (updated.unconfirmed_email) {
        // Devise's `update_needs_confirmation`.
        toast.success(
          'We need to verify your new email address. Please check your email and follow the confirm link to confirm your new email address.',
        )
      }
    } catch (e) {
      toast.error(e instanceof SecurityError ? e.message : 'Could not change your email address')
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">Email address</h2>
      {current.unconfirmed_email && (
        <p className="text-muted-foreground text-sm">
          Waiting for you to confirm {current.unconfirmed_email}. Until then, mail goes to{' '}
          {current.email}.
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
          <Label htmlFor="account-email">Email address</Label>
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
          <Label htmlFor="account-email-password">Current password</Label>
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
          Save changes
        </Button>
      </form>
    </section>
  )
}
