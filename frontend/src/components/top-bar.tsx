import { useEffect, useState } from 'react'
import { Link, NavLink, useLocation, useNavigate } from 'react-router-dom'
import {
  Bell,
  Check,
  Bookmark,
  Compass,
  Globe,
  MessageCircle,
  Home,
  Info,
  Laptop,
  LogIn,
  LogOut,
  Moon,
  MoreHorizontal,
  PenLine,
  Search,
  Settings,
  Shield,
  Sun,
  User,
  UserPlus,
  Users,
} from 'lucide-react'

import { toast } from 'sonner'
import { getInstance } from '../api.ts'
import { beginLogin, getSavedAccounts, getToken, logout, switchAccount } from '../auth.ts'
import { clearMe, getMeAccount, loadMe, type MeAccount } from '../me.ts'
import { Button } from '@/components/ui/button.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu.tsx'
import { useTheme } from '@/components/theme-provider.tsx'
import { ModeToggle } from '@/components/mode-toggle.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { useUnreadNotifications } from '../hooks/use-unread-notifications.ts'
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
  useSidebar,
} from '@/components/ui/sidebar.tsx'
import { cn } from '@/lib/utils.ts'
import { firstAdminSection } from '@/lib/admin-sections.ts'

// Roomier rows and a full-width pill for the current one, matching the weight
// 5.0 gives the rail now that it carries fewer things.
const navLink =
  'text-muted-foreground hover:text-foreground hover:bg-muted/60 flex w-full items-center gap-3 rounded-full px-3 py-2 text-[0.95rem] font-medium no-underline'
const activeNavLink = 'bg-muted text-foreground font-semibold'

type IconType = React.ComponentType<{ className?: string }>
type NavItem = {
  to: string
  end?: boolean
  icon: IconType
  label: string
  badge?: number
  // For a row that owns more than its own path — signed out, Local owns "/".
  matchAlso?: (pathname: string) => boolean
}

// The unread count, capped by the server. Past 99 the exact number stops
// meaning anything a badge can use.
function Badge({ count }: { count: number }) {
  if (count <= 0) return null
  return (
    <span
      className="bg-primary text-primary-foreground ml-auto rounded-full px-1.5 py-0.5 text-xs leading-none font-medium"
      aria-label={`${count} unread`}
    >
      {count > 99 ? '99+' : count}
    </span>
  )
}

function useNavItems(
  token: string | null,
  unread: number,
  account: MeAccount | null,
): NavItem[] {
  // The rail lists the places you read. On Mastodon that middle section is
  // custom feeds; eunha has none, and copying the shape around an absent
  // feature leaves a rail that is mostly empty. What eunha has instead is
  // three timelines, which were a tab strip inside the column — a leftover
  // from a thinner sidebar, and on a small invite-only server the local feed
  // is the community rather than a curiosity. So they live here.
  if (!token) {
    return [
      // Signed out, "/" *is* the local timeline, so that row owns both paths.
      { to: '/local', icon: Users, label: 'Local', matchAlso: (p) => p === '/' },
      { to: '/public', icon: Globe, label: 'Federated' },
      { to: '/explore', icon: Compass, label: 'Explore' },
      { to: '/about', icon: Info, label: 'About' },
    ]
  }
  // Moderation appears for a role that can open at least one of its pages —
  // Mastodon's web client shows its Moderation and Administration links on
  // the same kind of check. The row owns every `/admin` path.
  const moderation: NavItem[] =
    account?.permissions && firstAdminSection(account.permissions)
      ? [
          {
            to: '/admin',
            icon: Shield,
            label: 'Moderation',
            matchAlso: (p) => p.startsWith('/admin/'),
          },
        ]
      : []
  return [
    { to: '/', end: true, icon: Home, label: 'Home' },
    { to: '/local', icon: Users, label: 'Local' },
    { to: '/public', icon: Globe, label: 'Federated' },
    { to: '/search', icon: Search, label: 'Search' },
    { to: '/explore', icon: Compass, label: 'Explore' },
    { to: '/notifications', icon: Bell, label: 'Notifications', badge: unread },
    { to: '/messages', icon: MessageCircle, label: 'Messages' },
    { to: '/bookmarks', icon: Bookmark, label: 'Saved' },
    ...moderation,
  ]
}

// The server's name leads, not the software's. Mastodon 5.0 demotes its own
// logo so an instance reads as itself with a "powered by" line underneath;
// eunha has always shown the instance title here, and this gives it the weight
// that decision implies.
function ServerMark({
  title,
  icon,
  onNavigate,
}: {
  title: string
  icon?: string | null
  onNavigate?: () => void
}) {
  return (
    <Link
      to="/"
      onClick={onNavigate}
      className="flex items-center gap-2.5 px-2 no-underline"
    >
      {icon && (
        <img src={icon} alt="" className="size-9 shrink-0 rounded-lg object-cover" />
      )}
      <span className="min-w-0">
        <span className="block truncate text-lg font-semibold">{title}</span>
        <span className="text-muted-foreground block text-xs">powered by eunha</span>
      </span>
    </Link>
  )
}

// Everything about the signed-in person, in one card at the foot of the rail:
// who you are, and the menu that used to be four separate controls.
function AccountCard({
  account,
  onNavigate,
}: {
  account: MeAccount
  onNavigate?: () => void
}) {
  const { setTheme } = useTheme()
  return (
    <div className="flex items-center gap-2 rounded-lg border p-2">
      <Link
        to={`/@${account.acct}`}
        onClick={onNavigate}
        className="flex min-w-0 flex-1 items-center gap-2 no-underline"
      >
        <Avatar className="size-8">
          <AvatarImage src={account.avatar} alt="" />
          <AvatarFallback>
            {account.displayName.slice(0, 1).toUpperCase()}
          </AvatarFallback>
        </Avatar>
        <span className="min-w-0">
          <span className="block truncate text-sm font-medium">
            {account.displayName}
          </span>
          <span className="text-muted-foreground block truncate text-xs">
            @{account.acct}
          </span>
        </span>
      </Link>
      <DropdownMenu>
        <DropdownMenuTrigger
          aria-label="Account menu"
          className="text-muted-foreground hover:text-foreground hover:bg-muted focus-visible:ring-ring flex size-11 shrink-0 items-center justify-center rounded-md outline-none focus-visible:ring-2"
        >
          <MoreHorizontal className="size-5" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          {getSavedAccounts().map((saved) => (
            <DropdownMenuItem
              key={saved.account.id}
              disabled={saved.account.id === account.id}
              onClick={() => switchAccount(saved.account.id)}
            >
              {saved.account.id === account.id ? <Check /> : <User />}
              @{saved.account.acct}
            </DropdownMenuItem>
          ))}
          <DropdownMenuItem onClick={() => beginLogin(true).catch(() => toast.error('Could not start sign-in'))}>
            <UserPlus /> Add account
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem render={<Link to={`/@${account.acct}`} />}>
            <User /> Profile
          </DropdownMenuItem>
          <DropdownMenuItem render={<Link to="/settings" />}>
            <Settings /> Settings
          </DropdownMenuItem>
          <DropdownMenuItem onClick={() => setTheme('light')}>
            <Sun /> Light
          </DropdownMenuItem>
          <DropdownMenuItem onClick={() => setTheme('dark')}>
            <Moon /> Dark
          </DropdownMenuItem>
          <DropdownMenuItem onClick={() => setTheme('system')}>
            <Laptop /> System
          </DropdownMenuItem>
          <DropdownMenuItem
            variant="destructive"
            onClick={() => {
              logout()
              clearMe()
              location.assign('/')
            }}
          >
            <LogOut /> Sign out
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  )
}

function RailFooter({ domain, onNavigate }: { domain: string; onNavigate?: () => void }) {
  return (
    <p className="text-muted-foreground flex flex-wrap items-center gap-x-2 px-2 text-xs">
      <span className="truncate">{domain}</span>
      <Link to="/about" onClick={onNavigate} className="no-underline hover:underline">
        About
      </Link>
    </p>
  )
}

function AuthButton({
  token,
  size = 'sm',
}: {
  token: string | null
  size?: React.ComponentProps<typeof Button>['size']
}) {
  if (token) {
    return (
      <Button
        variant="ghost"
        size={size}
        onClick={() => {
          logout()
          clearMe()
          location.assign('/')
        }}
      >
        <LogOut /> Sign out
      </Button>
    )
  }
  return (
    <Button size={size} onClick={() => beginLogin()}>
      <LogIn /> Sign in
    </Button>
  )
}

function SignUpButton({ className }: { className?: string }) {
  const navigate = useNavigate()
  return (
    <Button className={className} onClick={() => navigate('/signup')}>
      <UserPlus /> Create account
    </Button>
  )
}

// The wide-screen rail: a fixed sidebar floating in the left margin of the
// centered column. Shown at `md` and up (see `.sidebar-frame`).
function DesktopRail({
  token,
  title,
  domain,
  icon,
  account,
  registrationsOpen,
}: {
  token: string | null
  title: string
  domain: string
  icon: string | null
  account: MeAccount | null
  registrationsOpen: boolean
}) {
  const { openCompose } = useComposeModal()
  const unread = useUnreadNotifications(token)
  const navItems = useNavItems(token, unread, account)
  const { pathname } = useLocation()

  return (
    <aside className="sidebar-frame">
      <ServerMark title={title} icon={icon} />

      <nav className="mt-5 flex flex-col gap-0.5">
        {/* Composing leads the rail as a row rather than a filled button —
            5.0's "New post". It reads as the first thing you can do here
            instead of an ornament floating under the navigation. */}
        {token && (
          <button type="button" className={navLink} onClick={() => openCompose()}>
            <PenLine className="size-5" /> New post
          </button>
        )}
        {navItems.map((item) => (
          <NavLink
            key={item.to}
            to={item.to}
            end={item.end}
            className={({ isActive }) =>
              cn(
                navLink,
                (isActive || item.matchAlso?.(pathname)) && activeNavLink,
              )
            }
          >
            <item.icon className="size-5" /> {item.label}
            {item.badge ? (
              <span className="ml-auto">
                <Badge count={item.badge} />
              </span>
            ) : null}
          </NavLink>
        ))}
      </nav>

      {!token && registrationsOpen && <SignUpButton className="mt-4 w-full" />}

      <div className="mt-auto flex flex-col gap-3 pt-4">
        {token && account ? (
          <AccountCard account={account} />
        ) : (
          !token && (
            <div className="flex items-center justify-between gap-2">
              <ModeToggle />
              <AuthButton token={token} />
            </div>
          )
        )}
        <RailFooter domain={domain} />
      </div>
    </aside>
  )
}

// The small-screen drawer: the shadcn Sidebar component, which renders inside a
// portaled Sheet toggled by `SidebarTrigger`. Only rendered below `xl`.
function MobileDrawer({
  token,
  title,
  domain,
  icon,
  account,
  registrationsOpen,
}: {
  token: string | null
  title: string
  domain: string
  icon: string | null
  account: MeAccount | null
  registrationsOpen: boolean
}) {
  const { openCompose } = useComposeModal()
  const { setOpenMobile } = useSidebar()
  const location = useLocation()
  const unread = useUnreadNotifications(token)
  const navItems = useNavItems(token, unread, account)
  const close = () => setOpenMobile(false)
  const isActive = (to: string, end?: boolean) =>
    end
      ? location.pathname === to
      : location.pathname === to || location.pathname.startsWith(`${to}/`)

  return (
    <Sidebar>
      <SidebarHeader>
        <ServerMark title={title} icon={icon} onNavigate={close} />
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarMenu>
            {token && (
              <SidebarMenuItem>
                <SidebarMenuButton
                  onClick={() => {
                    close()
                    openCompose()
                  }}
                >
                  <PenLine />
                  <span>New post</span>
                </SidebarMenuButton>
              </SidebarMenuItem>
            )}
            {navItems.map((item) => (
              <SidebarMenuItem key={item.to}>
                <SidebarMenuButton
                  isActive={
                    isActive(item.to, item.end) || !!item.matchAlso?.(location.pathname)
                  }
                  onClick={close}
                  render={<NavLink to={item.to} end={item.end} />}
                >
                  <item.icon />
                  <span>{item.label}</span>
                  {item.badge ? <Badge count={item.badge} /> : null}
                </SidebarMenuButton>
              </SidebarMenuItem>
            ))}
          </SidebarMenu>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <div className="flex flex-col gap-3">
          {token && account ? (
            <AccountCard account={account} onNavigate={close} />
          ) : (
            !token && (
              <>
                {registrationsOpen && <SignUpButton className="w-full" />}
                <div className="flex items-center justify-between gap-2">
                  <ModeToggle />
                  <AuthButton token={token} />
                </div>
              </>
            )
          )}
          <RailFooter domain={domain} onNavigate={close} />
        </div>
      </SidebarFooter>
    </Sidebar>
  )
}

function MobileHeader({
  token,
  title,
  registrationsOpen,
}: {
  token: string | null
  title: string
  registrationsOpen: boolean
}) {
  const { openCompose } = useComposeModal()
  const navigate = useNavigate()

  return (
    // Even padding rather than the old `pb-2` with none above: the bar's rule
    // sits directly on the column header's below it, and a row taller on one
    // side than the other reads as a mistake once there is nothing between
    // them. `.mobile-header` supplies the horizontal half — see styles.css,
    // which explains why it cannot simply be `px-3` here.
    <header className="mobile-header flex items-center gap-2 border-b py-2 md:hidden">
      <SidebarTrigger aria-label="Open menu" />
      <Link to="/" className="min-w-0 truncate text-lg font-semibold no-underline">
        {title}
      </Link>
      <div className="ml-auto flex shrink-0 items-center gap-2">
        {token ? (
          <Button size="sm" onClick={() => openCompose()}>
            <PenLine /> New post
          </Button>
        ) : (
          <>
            {registrationsOpen && (
              <Button size="sm" onClick={() => navigate('/signup')}>
                <UserPlus /> Create account
              </Button>
            )}
            <Button
              variant={registrationsOpen ? 'ghost' : 'default'}
              size="sm"
              onClick={() => beginLogin()}
            >
              <LogIn /> Sign in
            </Button>
          </>
        )}
      </div>
    </header>
  )
}

function ComposeButton({ token }: { token: string | null }) {
  const { openCompose } = useComposeModal()
  if (!token) return null
  return (
    <button
      type="button"
      aria-label="Compose"
      className="bg-primary text-primary-foreground hover:bg-primary/90 fixed right-4 bottom-4 z-40 flex size-13 items-center justify-center rounded-full shadow-lg sm:right-6 sm:bottom-6"
      onClick={() => openCompose()}
    >
      <PenLine className="size-5" />
    </button>
  )
}

function TopBarInner({
  token,
  title,
  domain,
  icon,
  account,
  registrationsOpen,
}: {
  token: string | null
  title: string
  domain: string
  icon: string | null
  account: MeAccount | null
  registrationsOpen: boolean
}) {
  const { isMobile } = useSidebar()

  return (
    <>
      <DesktopRail
        token={token}
        title={title}
        domain={domain}
        icon={icon}
        account={account}
        registrationsOpen={registrationsOpen}
      />
      <MobileHeader
        token={token}
        title={title}
        registrationsOpen={registrationsOpen}
      />
      <ComposeButton token={token} />
      {/* Below `xl` the Sidebar renders as a Sheet drawer; above it the
          DesktopRail handles navigation, so skip the component's own rail. */}
      {isMobile && (
        <MobileDrawer
          token={token}
          title={title}
          domain={domain}
          icon={icon}
          account={account}
          registrationsOpen={registrationsOpen}
        />
      )}
    </>
  )
}

export function TopBar({ title }: { title?: string }) {
  const token = getToken()
  const [instanceTitle, setInstanceTitle] = useState<string | null>(() =>
    document.title === 'eunha' ? null : document.title,
  )
  const [account, setAccount] = useState<MeAccount | null>(() => getMeAccount())
  const [registrationsOpen, setRegistrationsOpen] = useState(false)
  const [domain, setDomain] = useState('')
  const [icon, setIcon] = useState<string | null>(null)
  const displayTitle = title ?? instanceTitle ?? ''

  useEffect(() => {
    getInstance()
      .then((instance) => {
        if (!title) setInstanceTitle(instance.title)
        setDomain(instance.domain)
        // `icon` is a list of sizes; the largest is still small enough to sit
        // in a 36px box, and an instance that has set none sends an empty one.
        const icons = instance.icon as { src?: string }[] | undefined
        setIcon(icons?.at(-1)?.src ?? null)
      })
      .catch(() => {})
  }, [title])

  // Only logged-out users see the sign-up affordance, so only they need the
  // instance's registration status.
  useEffect(() => {
    if (token) return
    let cancelled = false
    getInstance()
      .then((instance) => {
        if (!cancelled) setRegistrationsOpen(instance.registrations.enabled)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [token])

  useEffect(() => {
    if (!token) {
      setAccount(null)
      return
    }

    let cancelled = false
    loadMe(token)
      .then((me) => {
        if (!cancelled) setAccount(me)
      })
      .catch(() => {
        if (!cancelled) setAccount(null)
      })

    return () => {
      cancelled = true
    }
  }, [token])

  useEffect(() => {
    if (!displayTitle) return
    document.title = displayTitle
  }, [displayTitle])

  return (
    <SidebarProvider className="block min-h-0 w-full">
      <TopBarInner
        token={token}
        title={displayTitle}
        domain={domain}
        icon={icon}
        account={account}
        registrationsOpen={registrationsOpen}
      />
    </SidebarProvider>
  )
}
