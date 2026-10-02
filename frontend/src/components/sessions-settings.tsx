import { useCallback, useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  getAuthorizedApplications,
  getLoginActivities,
  getSessions,
  revokeAuthorizedApplication,
  revokeSession,
  type AuthorizedApplication,
  type LoginActivity,
  type Session,
} from '../security-api.ts'
import { Button } from '@/components/ui/button.tsx'

const date = (value: string | null) => (value ? new Date(value).toLocaleString() : '')

// `login_activities.authentication_methods.*`.
const METHODS: Record<string, string> = {
  otp: 'two-factor authentication app',
  password: 'password',
  sign_in_token: 'email security code',
  webauthn: 'security keys',
}

/** Mastodon's sessions list: the browsers signed in to the account pages. */
function Sessions({ token }: { token: string }) {
  const [sessions, setSessions] = useState<Session[] | null>(null)
  const reload = useCallback(() => {
    getSessions(token)
      .then(setSessions)
      .catch(() => {})
  }, [token])
  useEffect(reload, [reload])

  if (!sessions) return null
  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Sessions</h2>
      <p className="text-muted-foreground text-sm">
        These are the web browsers currently logged in to your account.
      </p>
      {sessions.length === 0 ? (
        <p className="text-muted-foreground text-sm">No browser is signed in.</p>
      ) : (
        <ul className="divide-y rounded border text-sm">
          {sessions.map((session) => (
            <li key={session.id} className="flex items-center justify-between gap-2 p-2">
              <span>
                <span title={session.user_agent}>{session.description}</span>
                <span className="text-muted-foreground">
                  {' '}
                  · {session.ip ?? 'unknown address'} · Last activity {date(session.updated_at)}
                </span>
              </span>
              {session.current ? (
                <span className="text-muted-foreground text-xs">Current session</span>
              ) : (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() =>
                    revokeSession(token, session.id)
                      .then(() => {
                        toast.success('Session successfully revoked')
                        reload()
                      })
                      .catch(() => toast.error('Could not revoke the session'))
                  }
                >
                  Revoke
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

/** `OAuth::AuthorizedApplicationsController#index`. */
function AuthorizedApplications({ token }: { token: string }) {
  const [apps, setApps] = useState<AuthorizedApplication[] | null>(null)
  const reload = useCallback(() => {
    getAuthorizedApplications(token)
      .then(setApps)
      .catch(() => {})
  }, [token])
  useEffect(reload, [reload])

  if (!apps) return null
  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Authorized apps</h2>
      <p className="text-muted-foreground text-sm">
        These are applications that can access your account using the API. If there are
        applications you do not recognize here, or an application is misbehaving, you can revoke
        its access.
      </p>
      <ul className="divide-y rounded border text-sm">
        {apps.map((app) => (
          <li key={app.id} className="flex items-start justify-between gap-2 p-2">
            <span className="space-y-0.5">
              <span className="block font-medium">
                {app.website ? (
                  <a href={app.website} target="_blank" rel="noopener noreferrer">
                    {app.name}
                  </a>
                ) : (
                  app.name
                )}
              </span>
              <span className="text-muted-foreground block text-xs">
                {app.last_used_at
                  ? `Last used on ${new Date(app.last_used_at).toLocaleDateString()}`
                  : 'Never used'}{' '}
                · Authorized on {new Date(app.created_at).toLocaleDateString()}
              </span>
              <span className="text-muted-foreground block text-xs">{app.scopes.join(', ')}</span>
            </span>
            {!app.superapp && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  if (!window.confirm('Are you sure?')) return
                  revokeAuthorizedApplication(token, app.id)
                    .then(() => {
                      toast.success('Revoked')
                      reload()
                    })
                    .catch(() => toast.error('Could not revoke the app'))
                }}
              >
                Revoke
              </Button>
            )}
          </li>
        ))}
      </ul>
    </section>
  )
}

/** `Settings::LoginActivitiesController#index`. */
function AuthenticationHistory({ token }: { token: string }) {
  const [items, setItems] = useState<LoginActivity[]>([])
  const [more, setMore] = useState(true)
  const [open, setOpen] = useState(false)

  const load = (maxId?: string) =>
    getLoginActivities(token, maxId)
      .then((page) => {
        setItems((prev) => (maxId ? [...prev, ...page] : page))
        setMore(page.length > 0)
      })
      .catch(() => setMore(false))

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Authentication history</h2>
      <p className="text-muted-foreground text-sm">
        If you see activity that you don&apos;t recognize, consider changing your password and
        enabling two-factor authentication.
      </p>
      {!open ? (
        <Button
          size="sm"
          variant="secondary"
          onClick={() => {
            setOpen(true)
            void load()
          }}
        >
          View authentication history
        </Button>
      ) : items.length === 0 ? (
        <p className="text-muted-foreground text-sm">No authentication history available</p>
      ) : (
        <>
          <ul className="divide-y rounded border text-sm">
            {items.map((item) => {
              const method =
                METHODS[item.authentication_method ?? ''] ??
                item.provider ??
                item.authentication_method ??
                ''
              return (
                <li key={item.id} className="p-2">
                  {item.success ? 'Successful' : 'Failed'} sign-in
                  {item.success ? '' : ' attempt'} with {method} from {item.ip ?? 'an unknown address'}{' '}
                  <span className="text-muted-foreground">
                    ({item.browser}) · {date(item.created_at)}
                  </span>
                </li>
              )
            })}
          </ul>
          {more && (
            <Button size="sm" variant="ghost" onClick={() => void load(items[items.length - 1].id)}>
              Older
            </Button>
          )}
        </>
      )}
    </section>
  )
}

export function SessionsSettings({ token }: { token: string }) {
  return (
    <>
      <Sessions token={token} />
      <AuthorizedApplications token={token} />
      <AuthenticationHistory token={token} />
    </>
  )
}
