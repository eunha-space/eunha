import { useEffect, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { Ban, KeyRound } from 'lucide-react'
import { toast } from 'sonner'

import {
  ApiError,
  deleteAccount,
  getOwnEmailSubscriptions,
  setOwnEmailSubscriptions,
  type OwnEmailSubscriptions,
} from '../eunha-api.ts'
import { beginLogin, getToken, logout } from '../auth.ts'
import { isAdvancedLayout, setAdvancedLayout } from '../lib/panes.ts'
import { clearMe, getMeAccount } from '../me.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Label } from '@/components/ui/label.tsx'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog.tsx'

// Mastodon's `deletes.warning.*`, shown before the challenge.
const WARNINGS = [
  'You will not be able to restore or reactivate your account',
  'Your username will remain unavailable',
  'Your posts and other data will be permanently removed',
  'Content that has been cached by other servers may persist',
]

function DeleteAccount({ token }: { token: string }) {
  const navigate = useNavigate()
  const [password, setPassword] = useState('')
  const [confirming, setConfirming] = useState(false)
  const [deleting, setDeleting] = useState(false)

  const submit = async () => {
    setDeleting(true)
    try {
      await deleteAccount(token, { password })
      // The account is marked deleted the moment that returns, so this token
      // is already dead — drop the local session rather than let the next
      // request fail on its own.
      logout()
      clearMe()
      setConfirming(false)
      toast.success('Your account was successfully deleted')
      navigate('/')
    } catch (e) {
      setConfirming(false)
      toast.error(
        e instanceof ApiError && e.status === 401
          ? 'The information you entered was not correct'
          : 'Could not delete your account',
      )
    } finally {
      setDeleting(false)
    }
  }

  return (
    <section className="border-destructive/40 space-y-3 rounded-lg border p-4">
      <div>
        <h2 className="text-destructive font-semibold">Delete account</h2>
        <p className="text-muted-foreground text-sm">
          Before proceeding, please read these notes carefully:
        </p>
      </div>
      <ul className="text-muted-foreground list-disc space-y-1 pl-5 text-sm">
        {WARNINGS.map((warning) => (
          <li key={warning}>{warning}</li>
        ))}
      </ul>
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault()
          setConfirming(true)
        }}
      >
        <div className="space-y-1">
          <Label htmlFor="delete-password">Current password</Label>
          <Input
            id="delete-password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
          <p className="text-muted-foreground text-xs">
            Enter your current password to verify your identity.
          </p>
        </div>
        <Button type="submit" variant="destructive" disabled={!password}>
          Delete account
        </Button>
      </form>

      <AlertDialog open={confirming} onOpenChange={setConfirming}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Delete your account?</AlertDialogTitle>
            <AlertDialogDescription>
              This cannot be undone. Your posts and other data are removed, and
              your username stays unavailable.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={deleting}>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              disabled={deleting}
              onClick={submit}
            >
              {deleting ? 'Deleting…' : 'Delete account'}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </section>
  )
}

/**
 * Mastodon's "Send posts via email", on its privacy settings page: shown while
 * the feature is enabled to an account whose role may use it.
 */
function EmailSubscriptions({ token }: { token: string }) {
  const [own, setOwn] = useState<OwnEmailSubscriptions | null>(null)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    getOwnEmailSubscriptions(token)
      .then(setOwn)
      .catch(() => {})
  }, [token])

  if (!own?.available) return null

  const toggle = async (on: boolean) => {
    setSaving(true)
    try {
      setOwn(await setOwnEmailSubscriptions(token, on))
    } catch {
      toast.error('Could not save the setting')
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Send posts via email</h2>
      <p className="text-muted-foreground text-sm">
        Add an email sign-up form to your profile that appears for logged-out users. When
        visitors enter their email address and opt in, they get email updates for your public
        posts.
      </p>
      {(own.subscribers > 0 || own.enabled) && (
        <p className="text-sm">
          {own.enabled ? 'Active' : 'Inactive'} · {own.subscribers}{' '}
          {own.subscribers === 1 ? 'subscriber' : 'subscribers'}
        </p>
      )}
      <Label className="text-sm font-normal">
        <Switch checked={own.enabled} disabled={saving} onCheckedChange={toggle} />
        Offer email subscriptions on my profile
      </Label>
    </section>
  )
}

export default function Settings() {
  const token = getToken()
  const [advanced, setAdvanced] = useState(() => isAdvancedLayout())
  const me = getMeAccount()

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-1 text-lg font-bold">Settings</h1>
      <p className="text-muted-foreground mb-4 text-sm">
        {me ? `Signed in as @${me.acct}` : 'Your account'}
      </p>

      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">
            Sign in to manage your account.
          </p>
          <Button size="sm" onClick={() => beginLogin()}>
            Sign in
          </Button>
        </div>
      ) : (
        <div className="space-y-4">
          {/* Password changes still live on the server-rendered account pages. */}
          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">Password</h2>
            <p className="text-muted-foreground text-sm">
              Change your password on the account page.
            </p>
            <Button variant="secondary" size="sm" render={<a href="/account/password" />}>
              <KeyRound /> Change password
            </Button>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">Layout</h2>
            <p className="text-muted-foreground text-sm">
              Show several timelines side by side instead of one column. Stored
              in this browser, and only used on a wide one — a narrow screen
              stays on the single column whatever this says.
            </p>
            <Label className="text-sm font-normal">
              <Switch
                checked={advanced}
                onCheckedChange={(on) => {
                  setAdvancedLayout(on)
                  setAdvanced(on)
                }}
              />
              Advanced layout
            </Label>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">Blocked and muted</h2>
            <p className="text-muted-foreground text-sm">
              Review the accounts you have blocked or muted, and undo either.
            </p>
            <Button variant="secondary" size="sm" render={<Link to="/blocked" />}>
              <Ban /> Blocked and muted
            </Button>
          </section>

          <EmailSubscriptions token={token} />

          <DeleteAccount token={token} />
        </div>
      )}
    </div>
  )
}
