import { useEffect, useState, type ReactNode } from 'react'
import { NavLink, useLocation } from 'react-router-dom'

import { can, type Permission } from '../../admin-api.ts'
import { ADMIN_SECTIONS, type AdminSection } from '../../lib/admin-sections.ts'
import { beginLogin, getToken } from '../../auth.ts'
import { getMeAccount, loadMe } from '../../me.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { cn } from '@/lib/utils.ts'

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

const tab =
  'border-b-2 border-transparent px-3 py-2 text-sm font-medium whitespace-nowrap text-muted-foreground no-underline hover:text-foreground'
const tabClass = ({ isActive }: { isActive: boolean }) =>
  cn(tab, isActive && 'border-primary text-foreground')

const subTab =
  'rounded-full px-3 py-1 text-sm whitespace-nowrap text-muted-foreground no-underline hover:bg-muted/60 hover:text-foreground'
const subTabClass = ({ isActive }: { isActive: boolean }) =>
  cn(subTab, isActive && 'bg-muted text-foreground font-medium')

/**
 * The frame every moderation page sits in: the rail, the section tabs, an
 * optional second row of tabs within the section, and the page itself.
 *
 * It gates on the permission the page's endpoints need, so an account that
 * lacks it is told so rather than shown a page of refusals. The server checks
 * again on every call; this only saves a moderator the round trip.
 */
export function AdminLayout({
  title,
  permission,
  sub,
  actions,
  children,
}: {
  title: string
  permission: Permission
  /** Tabs within the section, e.g. domain blocks and domain allows. */
  sub?: { to: string; label: string; end?: boolean }[]
  /** Controls beside the title. */
  actions?: ReactNode
  children: ReactNode
}) {
  const token = getToken()
  const permissions = useRolePermissions()
  const { pathname } = useLocation()

  let body: ReactNode
  if (!token) {
    body = (
      <Card>
        <CardContent className="space-y-3 py-6 text-center">
          <p className="text-muted-foreground text-sm">
            Sign in with a moderator account to see this page.
          </p>
          <Button onClick={() => void beginLogin()}>Sign in</Button>
        </CardContent>
      </Card>
    )
  } else if (permissions === null) {
    body = <p className="text-muted-foreground text-sm">Loading…</p>
  } else if (!can(permissions, permission)) {
    body = (
      <p className="text-muted-foreground text-sm">
        Your role does not allow you to see this page.
      </p>
    )
  } else {
    body = children
  }

  const sections =
    permissions === null ? [] : ADMIN_SECTIONS.filter((s) => can(permissions, s.permission))

  return (
    <div className="page-frame">
      <TopBar />
      <p className="text-muted-foreground text-xs font-medium tracking-wide uppercase">
        Moderation
      </p>
      {sections.length > 0 && (
        <nav
          aria-label="Moderation sections"
          // Wraps rather than scrolls: eight sections don't fit beside the
          // rail, and a sideways-scrolling row hides the ones past the edge.
          className="mb-3 flex flex-wrap gap-x-1 border-b"
        >
          {sections.map((s) => (
            <NavLink
              key={s.to}
              to={s.to}
              className={({ isActive }) =>
                tabClass({ isActive: isActive || sectionOwns(s, pathname) })
              }
            >
              {s.label}
            </NavLink>
          ))}
        </nav>
      )}
      {sub && token && permissions !== null && can(permissions, permission) && (
        <nav aria-label={`${title} views`} className="mb-3 flex flex-wrap gap-1">
          {sub.map((s) => (
            <NavLink key={s.to} to={s.to} end={s.end} className={subTabClass}>
              {s.label}
            </NavLink>
          ))}
        </nav>
      )}
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <h1 className="min-w-0 flex-1 truncate text-lg font-bold">{title}</h1>
        {actions}
      </div>
      {body}
    </div>
  )
}

// A section's tab stays lit across its sub-pages: Federation covers domain
// allows as well as blocks, Blocks covers three lists, Trends four.
const SECTION_PREFIXES: Record<string, string[]> = {
  '/admin/domain_blocks': ['/admin/domain_blocks', '/admin/domain_allows'],
  '/admin/ip_blocks': [
    '/admin/ip_blocks',
    '/admin/email_domain_blocks',
    '/admin/canonical_email_blocks',
  ],
  '/admin/trends/links': ['/admin/trends'],
  '/admin/email_subscriptions': ['/admin/email_subscriptions'],
  '/admin/terms_of_service': ['/admin/terms_of_service'],
}

function sectionOwns(section: AdminSection, pathname: string): boolean {
  return (SECTION_PREFIXES[section.to] ?? []).some(
    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`),
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
          ? 'This session was signed in before moderation access was asked for.'
          : error.replace(/^\w*Error: /, '')}
      </p>
      {scopes && (
        <Button size="sm" onClick={() => void beginLogin()}>
          Sign in again
        </Button>
      )}
    </div>
  )
}
