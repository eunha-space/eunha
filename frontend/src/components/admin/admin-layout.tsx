import { useEffect, useState, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { ChevronLeft } from 'lucide-react'

import { can, type Permission } from '../../admin-api.ts'
import { faspEnabled, instanceHosted, moderationSubscriptionsEnabled } from '../../admin-server-api.ts'
import { ADMIN_SECTIONS, type AdminSection } from '../../lib/admin-sections.ts'
import { beginLogin, getToken } from '../../auth.ts'
import { getMeAccount, loadMe } from '../../me.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/**
 * The signed-in account's role permissions, or `null` until they are known.
 *
 * Read from the cached account first so a moderator's pages render on first
 * paint, then refreshed: a role changed since the cache was written should
 * show here without signing out.
 */
export function useRolePermissions(): number | null {
  const token = getToken()
  const [permissions, setPermissions] = useState<number | null>(() => {
    const cached = getMeAccount()
    return cached && cached.permissions !== undefined ? cached.permissions : null
  })
  useEffect(() => {
    if (!token) return
    let cancelled = false
    loadMe(token).then((me) => {
      if (!cancelled) setPermissions(me?.permissions ?? 0)
    })
    return () => {
      cancelled = true
    }
  }, [token])
  return permissions
}

/**
 * Whether the instance has each experimental feature on, asked only of a role
 * that may manage federation, which is who their sections are for.
 */
function useFeatures(permissions: number | null): { fasp: boolean; lists: boolean } {
  const token = getToken()
  const [features, setFeatures] = useState({ fasp: false, lists: false })
  const allowed = permissions !== null && can(permissions, 'manage_federation')
  useEffect(() => {
    if (!token || !allowed) return
    let cancelled = false
    Promise.all([faspEnabled(token), moderationSubscriptionsEnabled(token)]).then(
      ([fasp, lists]) => {
        if (!cancelled) setFeatures({ fasp, lists })
      },
    )
    return () => {
      cancelled = true
    }
  }, [token, allowed])
  return features
}

/** Whether a hosting service runs the instance (see [`instanceHosted`]). */
export function useHosted(): boolean {
  const token = getToken()
  const [hosted, setHosted] = useState(false)
  useEffect(() => {
    if (!token) return
    let cancelled = false
    instanceHosted(token).then((h) => {
      if (!cancelled) setHosted(h)
    })
    return () => {
      cancelled = true
    }
  }, [token])
  return hosted
}

/** The sections this role may open, with the experimental ones it has on. */
export function useAdminSections(): AdminSection[] | null {
  const permissions = useRolePermissions()
  const features = useFeatures(permissions)
  const hosted = useHosted()
  if (permissions === null) return null
  return ADMIN_SECTIONS.filter(
    (s) =>
      can(permissions, s.permission) &&
      (s.feature !== 'fasp' || features.fasp) &&
      (s.feature !== 'moderation_subscriptions' || features.lists) &&
      !(s.server && hosted),
  )
}

/**
 * The frame every moderation page sits in: the way back to the list of
 * sections at `/admin`, the title, and the page itself.
 *
 * It gates on the permission the page's endpoints need, so an account that
 * lacks it is told so rather than shown a page of refusals. The server checks
 * again on every call; this only saves a moderator the round trip.
 */
export function AdminLayout({
  title,
  permission,
  actions,
  back = true,
  children,
}: {
  title: string
  permission: Permission
  /** Controls beside the title. */
  actions?: ReactNode
  /** Whether to link back to `/admin`, which `/admin` itself doesn't. */
  back?: boolean
  children: ReactNode
}) {
  const token = getToken()
  const permissions = useRolePermissions()

  let body: ReactNode
  if (!token) {
    body = (
      <Card>
        <CardContent className="space-y-3 py-6 text-center">
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.admin.admin_layout.sign_in_with_a_moderator_account', defaultMessage: 'Sign in with a moderator account to see this page.' })}
          </p>
          <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>
        </CardContent>
      </Card>
    )
  } else if (permissions === null) {
    body = <RowsSkeleton />
  } else if (!can(permissions, permission)) {
    body = (
      <p className="text-muted-foreground text-sm">
        {t({ id: 'eunha.admin.admin_layout.your_role_does_not_allow_you', defaultMessage: 'Your role does not allow you to see this page.' })}
      </p>
    )
  } else {
    body = children
  }

  return (
    <div className="page-frame">
      <TopBar />
      {back && (
        <Link to="/admin" className="text-muted-foreground hover:text-foreground mb-1 inline-flex items-center gap-1 text-sm no-underline">
          <ChevronLeft className="size-4" aria-hidden />
          {t({ id: 'navigation_bar.moderation', defaultMessage: 'Moderation' })}
        </Link>
      )}
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <h1 className="min-w-0 flex-1 truncate text-lg font-bold">{title}</h1>
        {actions}
      </div>
      {body}
    </div>
  )
}

/**
 * A list or page that failed to load. A token from before the web client asked
 * for the admin scopes is refused for that reason alone, and signing in again
 * is the whole fix — so that case gets the button.
 */
export function AdminError({ error }: { error: string | null }) {
  if (!error) return null
  const scopes = /outside the authorized scopes/i.test(error)
  return (
    <div className="text-destructive space-y-2 text-sm">
      <p>
        {scopes
          ? t({ id: 'eunha.admin.admin_layout.this_session_was_signed_in_before', defaultMessage: 'This session was signed in before moderation access was asked for.' })
          : error.replace(/^\w*Error: /, '')}
      </p>
      {scopes && (
        <Button size="sm" onClick={() => void beginLogin()}>
          {t({ id: 'eunha.admin.admin_layout.sign_in_again', defaultMessage: 'Sign in again' })}
        </Button>
      )}
    </div>
  )
}
