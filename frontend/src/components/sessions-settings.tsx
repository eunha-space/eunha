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
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'

const date = (value: string | null) => (value ? new Date(value).toLocaleString(currentLocale()) : '')
const day = (value: string) => new Date(value).toLocaleDateString(currentLocale())

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
  const [failed, setFailed] = useState(false)
  const reload = useCallback(() => {
    getSessions(token)
      .then(setSessions)
      .catch(() => setFailed(true))
  }, [token])
  useEffect(reload, [reload])

  if (!sessions) return failed ? null : <SectionSkeleton rows={3} />
  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'mastodon.sessions.title', defaultMessage: 'Sessions' })}</h2>
      <p className="text-muted-foreground text-sm">
        {t({ id: 'eunha.sessions_settings.these_are_the_web_browsers_currently', defaultMessage: 'These are the web browsers currently logged in to your account.' })}
      </p>
      {sessions.length === 0 ? (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.sessions_settings.no_browser_is_signed_in', defaultMessage: 'No browser is signed in.' })}</p>
      ) : (
        <ul className="divide-y rounded border text-sm">
          {sessions.map((session) => (
            <li key={session.id} className="flex items-center justify-between gap-2 p-2">
              <span>
                <span title={session.user_agent}>{session.description}</span>
                <span className="text-muted-foreground">
                  {' '}
                  · {session.ip ?? t({ id: 'eunha.sessions_settings.unknown_address', defaultMessage: 'unknown address' })} ·{' '}
                  {t({ id: 'mastodon.sessions.activity', defaultMessage: 'Last activity' })} {date(session.updated_at)}
                </span>
              </span>
              {session.current ? (
                <span className="text-muted-foreground text-xs">{t({ id: 'mastodon.sessions.current_session', defaultMessage: 'Current session' })}</span>
              ) : (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() =>
                    revokeSession(token, session.id)
                      .then(() => {
                        toast.success(t({ id: 'mastodon.sessions.revoke_success', defaultMessage: 'Session successfully revoked' }))
                        reload()
                      })
                      .catch(() => toast.error(t({ id: 'eunha.sessions_settings.could_not_revoke_the_session', defaultMessage: 'Could not revoke the session' })))
                  }
                >
                  {t({ id: 'mastodon.sessions.revoke', defaultMessage: 'Revoke' })}
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
  const [failed, setFailed] = useState(false)
  const reload = useCallback(() => {
    getAuthorizedApplications(token)
      .then(setApps)
      .catch(() => setFailed(true))
  }, [token])
  useEffect(reload, [reload])

  if (!apps) return failed ? null : <SectionSkeleton rows={3} />
  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'mastodon.settings.authorized_apps', defaultMessage: 'Authorized apps' })}</h2>
      <p className="text-muted-foreground text-sm">
        {t({ id: 'mastodon.doorkeeper.authorized_applications.index.description_html', defaultMessage: 'These are applications that can access your account using the API. If there are applications you do not recognize here, or an application is misbehaving, you can revoke its access.' })}
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
                  ? t({ id: 'mastodon.doorkeeper.authorized_applications.index.last_used_at', defaultMessage: 'Last used on {date}' }, { date: day(app.last_used_at) })
                  : t({ id: 'mastodon.doorkeeper.authorized_applications.index.never_used', defaultMessage: 'Never used' })}{' '}
                ·{' '}
                {t({ id: 'mastodon.doorkeeper.authorized_applications.index.authorized_at', defaultMessage: 'Authorized on {date}' }, { date: day(app.created_at) })}
              </span>
              <span className="text-muted-foreground block text-xs">{app.scopes.join(', ')}</span>
            </span>
            {!app.superapp && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  if (!window.confirm(t({ id: 'mastodon.admin.accounts.are_you_sure', defaultMessage: 'Are you sure?' }))) return
                  revokeAuthorizedApplication(token, app.id)
                    .then(() => {
                      toast.success(t({ id: 'eunha.sessions_settings.revoked', defaultMessage: 'Revoked' }))
                      reload()
                    })
                    .catch(() => toast.error(t({ id: 'eunha.sessions_settings.could_not_revoke_the_app', defaultMessage: 'Could not revoke the app' })))
                }}
              >
                {t({ id: 'mastodon.sessions.revoke', defaultMessage: 'Revoke' })}
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
      <h2 className="font-semibold">{t({ id: 'mastodon.login_activities.title', defaultMessage: 'Authentication history' })}</h2>
      <p className="text-muted-foreground text-sm">
        {t({ id: 'mastodon.login_activities.description_html', defaultMessage: 'If you see activity that you don\'\'t recognize, consider changing your password and enabling two-factor authentication.' })}
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
          {t({ id: 'eunha.sessions_settings.view_authentication_history', defaultMessage: 'View authentication history' })}
        </Button>
      ) : items.length === 0 ? (
        <p className="text-muted-foreground text-sm">{t({ id: 'mastodon.login_activities.empty', defaultMessage: 'No authentication history available' })}</p>
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
                  {t(
                    item.success
                      ? { id: 'eunha.sessions_settings.successful_sign_in', defaultMessage: 'Successful sign-in with {method} from {ip}' }
                      : { id: 'eunha.sessions_settings.failed_sign_in', defaultMessage: 'Failed sign-in attempt with {method} from {ip}' },
                    { method, ip: item.ip ?? t({ id: 'eunha.sessions_settings.an_unknown_address', defaultMessage: 'an unknown address' }) },
                  )}{' '}
                  <span className="text-muted-foreground">
                    ({item.browser}) · {date(item.created_at)}
                  </span>
                </li>
              )
            })}
          </ul>
          {more && (
            <Button size="sm" variant="ghost" onClick={() => void load(items[items.length - 1].id)}>
              {t({ id: 'mastodon.pagination.older', defaultMessage: 'Older' })}
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
