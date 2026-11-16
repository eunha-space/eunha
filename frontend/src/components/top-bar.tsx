import { Fragment, useEffect, useRef, useState } from 'react'
import { Link, NavLink, useLocation } from 'react-router-dom'
import {
  Bell,
  Check,
  Bookmark,
  Compass,
  Globe,
  Hash,
  MessageCircle,
  Home,
  Info,
  List,
  LogIn,
  LogOut,
  MoreHorizontal,
  PenLine,
  Search,
  Settings,
  Shield,
  Ticket,
  User,
  UserPlus,
  Users,
} from 'lucide-react'

import { toast } from 'sonner'
import { defineMessages, FormattedMessage, useIntl } from 'react-intl'
import { getCachedInstance, getInstance } from '../api.ts'
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
import { ModeToggle, ThemeMenuPill } from '@/components/mode-toggle.tsx'
import { LanguageButton, LanguageSubmenu } from '@/components/language-menu.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { useAvailableInvites } from '../hooks/use-available-invites.ts'
import { useUnreadNotifications } from '../hooks/use-unread-notifications.ts'
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupLabel,
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
import { PERMISSION } from '../admin-api.ts'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { DisplayName } from '@/components/emoji.tsx'

// Roomier rows and a full-width pill for the current one, matching the weight
// 5.0 gives the rail now that it carries fewer things.
const navLink =
  'text-muted-foreground hover:text-foreground hover:bg-muted/60 flex w-full items-center gap-3 rounded-full px-3 py-2 text-[0.95rem] font-medium no-underline'
const activeNavLink = 'bg-muted text-foreground font-semibold'

// Where one of these means what a message of Mastodon's web client does, it
// carries Mastodon's id so Mastodon's translations reach it (see i18n.tsx).
const messages = defineMessages({
  home: { id: 'column.home', defaultMessage: 'Home' },
  notifications: { id: 'column.notifications', defaultMessage: 'Notifications' },
  messages: { id: 'tab_bar.messages', defaultMessage: 'Messages' },
  search: { id: 'navigation_bar.search', defaultMessage: 'Search' },
  explore: { id: 'tabs_bar.explore', defaultMessage: 'Explore' },
  local: { id: 'mastodon.admin.accounts.location.local', defaultMessage: 'Local' },
  federated: { id: 'eunha.navigation.federated', defaultMessage: 'Federated' },
  lists: { id: 'navigation_bar.lists', defaultMessage: 'Lists' },
  followedTags: { id: 'search_results.hashtags', defaultMessage: 'Hashtags' },
  saved: { id: 'tabs_bar.saved', defaultMessage: 'Saved' },
  invitePeople: { id: 'mastodon.invites.title', defaultMessage: 'Invite people' },
  moderation: { id: 'navigation_bar.moderation', defaultMessage: 'Moderation' },
  about: { id: 'column.about', defaultMessage: 'About' },
  footerAbout: { id: 'footer.about', defaultMessage: 'About' },
  feeds: { id: 'eunha.navigation.feeds', defaultMessage: 'Feeds' },
  you: { id: 'eunha.navigation.you', defaultMessage: 'You' },
  newPost: { id: 'tabs_bar.publish', defaultMessage: 'New post' },
  compose: { id: 'mastodon.user_mailer.welcome.post_action', defaultMessage: 'Compose' },
  poweredBy: { id: 'eunha.navigation.powered_by', defaultMessage: 'powered by eunha' },
  openMenu: { id: 'eunha.navigation.open_menu', defaultMessage: 'Open menu' },
  unread: { id: 'notifications.unread_with_count', defaultMessage: '{count} unread' },
  inviteCount: {
    id: 'eunha.navigation.invite_count',
    defaultMessage: '{count} available single-use invite links',
  },
  accountMenu: { id: 'eunha.account_menu.label', defaultMessage: 'Account menu' },
  addAccount: { id: 'eunha.account_menu.add_account', defaultMessage: 'Add account' },
  signInFailed: { id: 'eunha.account_menu.sign_in_failed', defaultMessage: 'Could not start sign-in' },
  profile: { id: 'eunha.account_menu.profile', defaultMessage: 'Profile' },
  settings: { id: 'tabs_bar.settings', defaultMessage: 'Settings' },
  signOut: { id: 'navigation_bar.logout', defaultMessage: 'Sign out' },
  signIn: { id: 'sign_in_banner.sign_in', defaultMessage: 'Sign in' },
  createAccount: { id: 'sign_in_banner.create_account', defaultMessage: 'Create account' },
})

type IconType = React.ComponentType<{ className?: string }>
type NavItem = {
  to: string
  end?: boolean
  icon: IconType
  label: string
  inviteCount?: number | null
  badge?: number
  // For a row that owns more than its own path — signed out, Local owns "/".
  matchAlso?: (pathname: string) => boolean
}

// The unread count, capped by the server. Past 99 the exact number stops
// meaning anything a badge can use.
function Badge({ count }: { count: number }) {
  const intl = useIntl()
  if (count <= 0) return null
  return (
    <span
      className="bg-primary text-primary-foreground ml-auto rounded-full px-1.5 py-0.5 text-xs leading-none font-medium"
      aria-label={intl.formatMessage(messages.unread, { count })}
    >
      {count > 99 ? '99+' : count}
    </span>
  )
}

function InviteCount({ count }: { count?: number | null }) {
  const intl = useIntl()
  return count ? <span className="bg-muted text-muted-foreground ml-auto rounded-md px-1.5 py-0.5 text-xs tabular-nums" aria-label={intl.formatMessage(messages.inviteCount, { count })}>{count}</span> : null
}

/**
 * Mastodon's `canViewFeed`, over a `*_live_feed_access` setting the instance
 * reports in `configuration.timelines_access`: `public` for anyone,
 * `authenticated` for the signed in, and otherwise for a role whose
 * permissions carry the `view_feeds` bit. Like Mastodon's client, it reads
 * that bit alone: `administrator`, which grants it on the server, does not
 * show the row by itself.
 */
function canViewFeed(
  signedIn: boolean,
  account: MeAccount | null,
  setting: string | undefined,
): boolean {
  switch (setting) {
    case 'public':
      return true
    case 'authenticated':
      return signedIn
    default:
      return ((account?.permissions ?? 0) & PERMISSION.view_feeds) === PERMISSION.view_feeds
  }
}

/**
 * `local_live_feed_access` and `remote_live_feed_access`, from the instance
 * the top bar last loaded.
 */
function liveFeedAccess(): { local?: string; remote?: string } {
  const configuration = getCachedInstance()?.configuration as
    | { timelinesAccess?: { liveFeeds?: { local?: string; remote?: string } } }
    | undefined
  return configuration?.timelinesAccess?.liveFeeds ?? {}
}

// A run of rows in the rail. The first group has no heading; the others are
// labelled so a long rail reads as a few short lists.
// `id` names a group the rail places something after.
type NavGroup = { id?: 'you'; label?: string; items: NavItem[] }

function useNavGroups(
  token: string | null,
  unread: number,
  account: MeAccount | null,
  invites: { count: number | null; hasAvailable: boolean },
): NavGroup[] {
  const intl = useIntl()
  const t = intl.formatMessage
  // The rail lists the places you read. On Mastodon that middle section is
  // custom feeds. eunha also links home and local timelines, which were a
  // tab strip inside the column — a leftover
  // from a thinner sidebar, and on a small invite-only server the local feed
  // is the community rather than a curiosity. So they live here. Local and
  // federated are shown as Mastodon shows its live feeds: to whoever the
  // server's `local_live_feed_access` and `remote_live_feed_access` let read
  // them.
  const access = liveFeedAccess()
  const local = canViewFeed(!!token, account, access.local)
  const federated: NavItem[] = canViewFeed(!!token, account, access.remote)
    ? [{ to: '/public', icon: Globe, label: t(messages.federated) }]
    : []
  if (!token) {
    return [
      {
        items: [
          // Signed out, "/" *is* the local timeline, so that row owns both paths.
          ...(local
            ? [{ to: '/local', icon: Users, label: t(messages.local), matchAlso: (p: string) => p === '/' }]
            : []),
          ...federated,
          { to: '/explore', icon: Compass, label: t(messages.explore) },
          { to: '/about', icon: Info, label: t(messages.about) },
        ],
      },
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
            label: t(messages.moderation),
            matchAlso: (p) => p.startsWith('/admin/'),
          },
        ]
      : []
  const groups: NavGroup[] = [
    // Where you go most: your timeline, what is waiting for you, and finding
    // things.
    {
      items: [
        { to: '/', end: true, icon: Home, label: t(messages.home) },
        { to: '/notifications', icon: Bell, label: t(messages.notifications), badge: unread },
        { to: '/messages', icon: MessageCircle, label: t(messages.messages) },
        { to: '/search', icon: Search, label: t(messages.search) },
        { to: '/explore', icon: Compass, label: t(messages.explore) },
      ],
    },
    // The other timelines you can read.
    {
      label: t(messages.feeds),
      items: [
        ...(local ? [{ to: '/local', icon: Users, label: t(messages.local) }] : []),
        ...federated,
        { to: '/lists', icon: List, label: t(messages.lists), matchAlso: (path) => path.startsWith('/lists/') },
        { to: '/followed_tags', icon: Hash, label: t(messages.followedTags) },
      ],
    },
    // What is yours to keep or give out.
    {
      id: 'you',
      label: t(messages.you),
      items: [
        { to: '/bookmarks', icon: Bookmark, label: t(messages.saved) },
        ...(invites.hasAvailable || moderation.length > 0 ? [{
          to: '/invites', icon: Ticket,
          label: t(messages.invitePeople), inviteCount: invites.count,
        }] : []),
      ],
    },
    // Staff tools, set apart at the end.
    { items: moderation },
  ]
  return groups.filter((group) => group.items.length > 0)
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
        <span className="text-muted-foreground block text-xs">
          <FormattedMessage {...messages.poweredBy} />
        </span>
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
  const intl = useIntl()
  const cardRef = useRef<HTMLDivElement>(null)
  const [menuOpen, setMenuOpen] = useState(false)
  // An account cached before the still avatar was stored has none to show
  // until `loadMe` fills it in, and the animated one is not shown in its place.
  const avatar = useAnimatedImage(
    account.avatarStatic === undefined ? null : account.avatar,
    account.avatarStatic,
  )
  return (
    // While the menu is open the card squares its top corners and takes the
    // menu's surface, so the two read as one panel grown up out of the card
    // (see `.account-menu` in styles.css).
    <div
      ref={cardRef}
      data-menu-open={menuOpen || undefined}
      className="account-card flex items-center gap-2 rounded-lg border p-2"
    >
      <Link
        to={`/@${account.acct}`}
        onClick={onNavigate}
        className="flex min-w-0 flex-1 items-center gap-2 no-underline"
      >
        <Avatar className="size-8" {...avatar.hover}>
          <AvatarImage src={avatar.src} alt="" />
          <AvatarFallback>
            {account.displayName.slice(0, 1).toUpperCase()}
          </AvatarFallback>
        </Avatar>
        <span className="min-w-0">
          <DisplayName
            account={{ displayName: account.displayName, username: account.acct, emojis: account.emojis }}
            className="block truncate text-sm font-medium"
          />
          <span className="text-muted-foreground block truncate text-xs">
            @{account.acct}
          </span>
        </span>
      </Link>
      <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
        <DropdownMenuTrigger
          aria-label={intl.formatMessage(messages.accountMenu)}
          className="text-muted-foreground hover:text-foreground hover:bg-muted focus-visible:ring-ring flex size-11 shrink-0 items-center justify-center rounded-md outline-none focus-visible:ring-2"
        >
          <MoreHorizontal className="size-5" />
        </DropdownMenuTrigger>
        <DropdownMenuContent
          anchor={cardRef}
          side="top"
          align="center"
          sideOffset={-1}
          className="account-menu"
        >
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
          <DropdownMenuItem onClick={() => beginLogin(true).catch(() => toast.error(intl.formatMessage(messages.signInFailed)))}>
            <UserPlus /> <FormattedMessage {...messages.addAccount} />
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem render={<Link to={`/@${account.acct}`} />}>
            <User /> <FormattedMessage {...messages.profile} />
          </DropdownMenuItem>
          <DropdownMenuItem render={<Link to="/settings" />}>
            <Settings /> <FormattedMessage {...messages.settings} />
          </DropdownMenuItem>
          <ThemeMenuPill />
          <LanguageSubmenu />
          <DropdownMenuItem
            variant="destructive"
            onClick={async () => {
              await logout()
              clearMe()
              location.assign('/')
            }}
          >
            <LogOut /> <FormattedMessage {...messages.signOut} />
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
        <FormattedMessage {...messages.footerAbout} />
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
        onClick={async () => {
          await logout()
          clearMe()
          location.assign('/')
        }}
      >
        <LogOut /> <FormattedMessage {...messages.signOut} />
      </Button>
    )
  }
  return (
    <Button size={size} onClick={() => beginLogin()}>
      <LogIn /> <FormattedMessage {...messages.signIn} />
    </Button>
  )
}

// The sign-up form is a server page, as Mastodon's is, so apps and links
// made for Mastodon reach the same one.
function SignUpButton({ className }: { className?: string }) {
  return (
    <Button className={className} onClick={() => window.location.assign('/auth/signup')}>
      <UserPlus /> <FormattedMessage {...messages.createAccount} />
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
  const inviteCount = useAvailableInvites(token)
  const navGroups = useNavGroups(token, unread, account, inviteCount)
  const { pathname } = useLocation()

  return (
    <aside className="sidebar-frame">
      <ServerMark title={title} icon={icon} />

      <nav className="mt-5 flex flex-col gap-0.5">
        {navGroups.map((group, index) => (
          <Fragment key={group.label ?? index}>
            <div
              role="group"
              aria-label={group.label}
              className={cn('flex flex-col gap-0.5', index > 0 && 'mt-4')}
            >
              {group.label && (
                <span
                  aria-hidden
                  className="text-muted-foreground px-3 pb-1 text-xs font-medium tracking-wide uppercase"
                >
                  {group.label}
                </span>
              )}
              {group.items.map((item) => (
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
                  <InviteCount count={item.inviteCount} />
                  {item.badge ? (
                    <span className="ml-auto">
                      <Badge count={item.badge} />
                    </span>
                  ) : null}
                </NavLink>
              ))}
            </div>
            {/* Composing closes the member's own rows as a filled button, under
                You and above the staff tools, so it stands apart from the
                navigation without leading it. */}
            {token && group.id === 'you' && (
              <Button size="lg" className="mt-4 w-full rounded-full" onClick={() => openCompose()}>
                <PenLine /> <FormattedMessage {...messages.newPost} />
              </Button>
            )}
          </Fragment>
        ))}
      </nav>

      {!token && registrationsOpen && <SignUpButton className="mt-4 w-full" />}

      <div className="mt-auto flex flex-col gap-3 pt-4">
        {token && account ? (
          <AccountCard account={account} />
        ) : (
          !token && (
            <div className="flex items-center justify-between gap-2">
              <span className="flex items-center gap-1.5">
                <ModeToggle />
                <LanguageButton />
              </span>
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
  const inviteCount = useAvailableInvites(token)
  const navGroups = useNavGroups(token, unread, account, inviteCount)
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
        {navGroups.map((group, index) => (
          <Fragment key={group.label ?? index}>
            <SidebarGroup>
              {group.label && (
                <SidebarGroupLabel className="tracking-wide uppercase">{group.label}</SidebarGroupLabel>
              )}
              <SidebarMenu>
                {group.items.map((item) => (
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
                      <InviteCount count={item.inviteCount} />
                      {item.badge ? <Badge count={item.badge} /> : null}
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroup>
            {token && group.id === 'you' && (
              <div className="px-2 pb-2">
                <Button
                  size="lg"
                  className="w-full rounded-full"
                  onClick={() => {
                    close()
                    openCompose()
                  }}
                >
                  <PenLine /> <FormattedMessage {...messages.newPost} />
                </Button>
              </div>
            )}
          </Fragment>
        ))}
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
                  <span className="flex items-center gap-1.5">
                    <ModeToggle />
                    <LanguageButton />
                  </span>
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
  const intl = useIntl()

  return (
    // Even padding rather than the old `pb-2` with none above: the bar's rule
    // sits directly on the column header's below it, and a row taller on one
    // side than the other reads as a mistake once there is nothing between
    // them. `.mobile-header` supplies the horizontal half — see styles.css,
    // which explains why it cannot simply be `px-3` here.
    <header className="mobile-header flex items-center gap-2 border-b py-2 md:hidden">
      <SidebarTrigger aria-label={intl.formatMessage(messages.openMenu)} />
      <Link to="/" className="min-w-0 truncate text-lg font-semibold no-underline">
        {title}
      </Link>
      <div className="ml-auto flex shrink-0 items-center gap-2">
        {token ? (
          <Button size="sm" onClick={() => openCompose()}>
            <PenLine /> <FormattedMessage {...messages.newPost} />
          </Button>
        ) : (
          <>
            {registrationsOpen && (
              <Button size="sm" onClick={() => window.location.assign('/auth/signup')}>
                <UserPlus /> <FormattedMessage {...messages.createAccount} />
              </Button>
            )}
            <Button
              variant={registrationsOpen ? 'ghost' : 'default'}
              size="sm"
              onClick={() => beginLogin()}
            >
              <LogIn /> <FormattedMessage {...messages.signIn} />
            </Button>
          </>
        )}
      </div>
    </header>
  )
}

function ComposeButton({ token }: { token: string | null }) {
  const { openCompose } = useComposeModal()
  const intl = useIntl()
  if (!token) return null
  return (
    <button
      type="button"
      aria-label={intl.formatMessage(messages.compose)}
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

// The site title the server wrote into the page, read when the app loads.
// Not `<title>`: on a profile or a post that names the page first, and the
// rail would show the page's name until `/api/v2/instance` answered.
const serverTitle =
  document.querySelector<HTMLMetaElement>('meta[name="application-name"]')?.content || null

// `title` is the page's, for the browser tab only; the rail and the mobile
// header always name the server.
export function TopBar({ title }: { title?: string }) {
  const token = getToken()
  const [instance, setInstance] = useState(getCachedInstance)
  const [account, setAccount] = useState<MeAccount | null>(() => getMeAccount())
  const registrationsOpen = instance?.registrations.enabled ?? false
  const domain = instance?.domain ?? ''
  // `icon` is a list of sizes; the largest still fits the 36px box.
  const icons = instance?.icon as { src?: string }[] | undefined
  const icon = icons?.at(-1)?.src ?? null
  const siteTitle = instance?.title ?? serverTitle ?? ''
  // As the server writes it: the page's title before the site's.
  const documentTitle = title && siteTitle ? `${title} - ${siteTitle}` : (title ?? siteTitle)

  useEffect(() => {
    let cancelled = false
    getInstance()
      .then((instance) => {
        if (!cancelled) setInstance(instance)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [])

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
    if (!documentTitle) return
    document.title = documentTitle
  }, [documentTitle])

  return (
    <SidebarProvider className="block min-h-0 w-full">
      <TopBarInner
        token={token}
        title={siteTitle}
        domain={domain}
        icon={icon}
        account={account}
        registrationsOpen={registrationsOpen}
      />
    </SidebarProvider>
  )
}
