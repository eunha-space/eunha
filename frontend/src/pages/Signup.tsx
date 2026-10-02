import { useEffect, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'

import { getInstance } from '../api.ts'
import { signUp } from '../eunha-api.ts'
import { beginLogin } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'

// What masto does not type of `registrations`: Mastodon 4.4's minimum age and
// whether the reason for joining is required.
interface RegistrationExtras {
  minAge?: number | null
  reasonRequired?: boolean
}

export default function Signup() {
  const [params] = useSearchParams()
  const inviteFromUrl = params.get('invite')?.trim() ?? ''

  const [registrationsOpen, setRegistrationsOpen] = useState<boolean | null>(null)
  const [approvalRequired, setApprovalRequired] = useState(false)
  const [minAge, setMinAge] = useState<number | null>(null)
  const [reasonRequired, setReasonRequired] = useState(false)

  const [username, setUsername] = useState('')
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [invite, setInvite] = useState(inviteFromUrl)
  const [reason, setReason] = useState('')
  const [dateOfBirth, setDateOfBirth] = useState('')
  const [agreement, setAgreement] = useState(false)

  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState(false)

  useEffect(() => {
    getInstance()
      .then((instance) => {
        setRegistrationsOpen(instance.registrations.enabled)
        setApprovalRequired(instance.registrations.approvalRequired)
        const extras = instance.registrations as RegistrationExtras
        setMinAge(extras.minAge ?? null)
        setReasonRequired(extras.reasonRequired ?? false)
      })
      .catch(() => setRegistrationsOpen(false))
  }, [])

  const hasInvite = invite.trim().length > 0
  // Closed instances require an invite; approval-required instances ask for a
  // reason unless the invite bypasses approval.
  const inviteRequired = registrationsOpen === false && !hasInvite
  const needsReason = approvalRequired && !hasInvite

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setError(null)
    if (password !== confirm) {
      setError('Passwords do not match.')
      return
    }
    setSubmitting(true)
    try {
      await signUp({
        username: username.trim(),
        email: email.trim(),
        password,
        locale: navigator.language.split('-')[0] || 'en',
        invite_code: invite.trim() || undefined,
        reason: reason.trim() || undefined,
        agreement,
        date_of_birth: minAge !== null ? dateOfBirth : undefined,
      })
      setDone(true)
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <div className="page-frame">
      <TopBar />
      <div className="mx-auto max-w-sm">
        <h1 className="mb-1 text-lg font-bold">Create account</h1>

        {done ? (
          <div className="space-y-3">
            <div className="bg-muted rounded-lg border p-4 text-sm">
              Almost there — we sent a confirmation link to{' '}
              <span className="font-medium">{email}</span>. Click it to activate
              your account.
              {approvalRequired && !hasInvite && (
                <>
                  {' '}
                  After confirming, an admin will review your application.
                </>
              )}
            </div>
            <p className="text-muted-foreground text-sm">
              Already confirmed?{' '}
              <button
                className="text-primary font-medium"
                onClick={() => beginLogin()}
              >
                Sign in
              </button>
            </p>
          </div>
        ) : registrationsOpen === null ? (
          <p className="text-muted-foreground text-sm">Loading…</p>
        ) : (
          <form onSubmit={submit} className="space-y-3">
            <p className="text-muted-foreground text-sm">
              {inviteRequired
                ? 'This instance is invite-only. Enter your invite code to continue.'
                : approvalRequired && !hasInvite
                  ? 'Registrations are open by approval — tell us a bit about yourself.'
                  : 'Join this instance.'}
            </p>

            {error && <p className="text-destructive text-sm">{error}</p>}

            {registrationsOpen === false && (
              <div className="space-y-1">
                <Label htmlFor="invite">Invite code</Label>
                <Input
                  id="invite"
                  value={invite}
                  required
                  autoComplete="off"
                  onChange={(e) => setInvite(e.target.value)}
                />
              </div>
            )}

            <div className="space-y-1">
              <Label htmlFor="username">Username</Label>
              <Input
                id="username"
                value={username}
                required
                autoComplete="username"
                pattern="[a-zA-Z0-9_]+"
                title="Letters, numbers, and underscores only"
                onChange={(e) => setUsername(e.target.value)}
              />
            </div>

            <div className="space-y-1">
              <Label htmlFor="email">Email</Label>
              <Input
                id="email"
                type="email"
                value={email}
                required
                autoComplete="email"
                onChange={(e) => setEmail(e.target.value)}
              />
            </div>

            <div className="space-y-1">
              <Label htmlFor="password">Password</Label>
              <Input
                id="password"
                type="password"
                value={password}
                required
                minLength={8}
                autoComplete="new-password"
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>

            <div className="space-y-1">
              <Label htmlFor="confirm">Confirm password</Label>
              <Input
                id="confirm"
                type="password"
                value={confirm}
                required
                minLength={8}
                autoComplete="new-password"
                onChange={(e) => setConfirm(e.target.value)}
              />
            </div>

            {minAge !== null && (
              <div className="space-y-1">
                <Label htmlFor="date_of_birth">Date of birth</Label>
                <Input
                  id="date_of_birth"
                  type="date"
                  value={dateOfBirth}
                  required
                  autoComplete="bday"
                  onChange={(e) => setDateOfBirth(e.target.value)}
                />
                <p className="text-muted-foreground text-xs">
                  You must be at least {minAge} years old to sign up.
                </p>
              </div>
            )}

            {needsReason && (
              <div className="space-y-1">
                <Label htmlFor="reason">Why do you want to join?</Label>
                <Textarea
                  id="reason"
                  value={reason}
                  required={reasonRequired}
                  maxLength={420}
                  onChange={(e) => setReason(e.target.value)}
                />
              </div>
            )}

            <Label className="items-start text-sm font-normal">
              <Checkbox
                checked={agreement}
                onCheckedChange={(checked) => setAgreement(checked === true)}
              />
              <span>
                I have read and agree to the{' '}
                <Link to="/terms-of-service" target="_blank" className="underline">
                  terms of service
                </Link>{' '}
                and{' '}
                <Link to="/privacy-policy" target="_blank" className="underline">
                  privacy policy
                </Link>
              </span>
            </Label>

            <Button type="submit" className="w-full" disabled={submitting || !agreement}>
              {submitting
                ? 'Creating…'
                : needsReason
                  ? 'Apply for an account'
                  : 'Create account'}
            </Button>

            <p className="text-muted-foreground text-sm">
              Already have an account?{' '}
              <button
                type="button"
                className="text-primary font-medium"
                onClick={() => beginLogin()}
              >
                Sign in
              </button>
            </p>
            <p className="text-muted-foreground text-xs">
              <Link to="/about" className="underline">
                About this instance
              </Link>
            </p>
          </form>
        )}
      </div>
    </div>
  )
}
