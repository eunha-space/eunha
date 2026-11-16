import type { ComponentType, ReactNode } from 'react'
import { Link } from 'react-router-dom'
import {
  ArrowLeftRight, Bell, ChevronLeft, ChevronRight, EyeOff, History, KeyRound, Palette, PenLine,
} from 'lucide-react'

import { beginLogin, getToken } from '../auth.ts'
import { getMeAccount } from '../me.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import {
  Item, ItemActions, ItemContent, ItemDescription, ItemGroup, ItemMedia, ItemTitle,
} from '@/components/ui/item.tsx'
import { t } from '@/i18n.tsx'

/**
 * The settings pages, at Mastodon's paths for them and in the order its
 * `config/navigation.rb` lists them, so a link Mastodon or an app makes to one
 * lands on the matching page. `/settings` itself lists them.
 */
export const SETTINGS_PAGES: {
  to: string
  icon: ComponentType<{ className?: string }>
  label: () => string
  summary: () => string
}[] = [
  {
    to: '/settings/account',
    icon: KeyRound,
    label: () => t({ id: 'navigation_bar.account_settings', defaultMessage: 'Password and security' }),
    summary: () => t({ id: 'eunha.settings_layout.account_summary', defaultMessage: 'Email, password, two-factor, sessions' }),
  },
  {
    to: '/settings/privacy',
    icon: EyeOff,
    label: () => t({ id: 'navigation_bar.privacy_and_reach', defaultMessage: 'Privacy and reach' }),
    summary: () => t({ id: 'eunha.settings_layout.privacy_summary', defaultMessage: 'Discovery, blocks, filters' }),
  },
  {
    to: '/settings/preferences/appearance',
    icon: Palette,
    label: () => t({ id: 'mastodon.settings.appearance', defaultMessage: 'Appearance' }),
    summary: () => t({ id: 'eunha.settings_layout.appearance_summary', defaultMessage: 'Palette, layout, timelines' }),
  },
  {
    to: '/settings/preferences/posting_defaults',
    icon: PenLine,
    label: () => t({ id: 'mastodon.preferences.posting_defaults', defaultMessage: 'Posting defaults' }),
    summary: () => t({ id: 'eunha.settings_layout.posting_summary', defaultMessage: 'Language, quoting, collections' }),
  },
  {
    to: '/settings/preferences/notifications',
    icon: Bell,
    label: () => t({ id: 'column.notifications', defaultMessage: 'Notifications' }),
    summary: () => t({ id: 'eunha.settings_layout.notifications_summary', defaultMessage: 'Email and browser' }),
  },
  {
    to: '/settings/statuses_cleanup',
    icon: History,
    label: () => t({ id: 'navigation_bar.automated_deletion', defaultMessage: 'Automated post deletion' }),
    summary: () => t({ id: 'eunha.settings_layout.statuses_cleanup_summary', defaultMessage: 'Delete old posts on a schedule' }),
  },
  {
    to: '/settings/export',
    icon: ArrowLeftRight,
    label: () => t({ id: 'navigation_bar.import_export', defaultMessage: 'Import and export' }),
    summary: () => t({ id: 'eunha.settings_layout.import_export_summary', defaultMessage: 'Download your data, bring lists in' }),
  },
]

function SignIn() {
  return (
    <div className="space-y-2">
      <p className="text-muted-foreground text-sm">
        {t({ id: 'eunha.settings.sign_in_to_manage_your_account', defaultMessage: 'Sign in to manage your account.' })}
      </p>
      <Button size="sm" onClick={() => beginLogin()}>
        {t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}
      </Button>
    </div>
  )
}

/** `/settings`: who is signed in, and the settings pages to open. */
export function SettingsIndex() {
  const token = getToken()
  const me = getMeAccount()

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-1 text-lg font-bold">{t({ id: 'tabs_bar.settings', defaultMessage: 'Settings' })}</h1>
      <p className="text-muted-foreground mb-4 text-sm">
        {me ? t({ id: 'eunha.settings.signed_in_as', defaultMessage: 'Signed in as @{acct}' }, { acct: me.acct }) : t({ id: 'eunha.settings.your_account', defaultMessage: 'Your account' })}
      </p>
      {!token ? (
        <SignIn />
      ) : (
        <nav aria-label={t({ id: 'eunha.settings_layout.settings_pages', defaultMessage: 'Settings pages' })}>
          <ItemGroup className="gap-2">
            {SETTINGS_PAGES.map(({ to, icon: Icon, label, summary }) => (
              <div key={to} role="listitem">
                <Item variant="outline" render={<Link to={to} />} className="text-inherit no-underline">
                  <ItemMedia variant="icon" className="bg-muted size-9 rounded-md">
                    <Icon className="size-4" />
                  </ItemMedia>
                  <ItemContent>
                    <ItemTitle>{label()}</ItemTitle>
                    <ItemDescription>{summary()}</ItemDescription>
                  </ItemContent>
                  <ItemActions>
                    <ChevronRight className="text-muted-foreground size-4" aria-hidden />
                  </ItemActions>
                </Item>
              </div>
            ))}
          </ItemGroup>
        </nav>
      )}
    </div>
  )
}

/**
 * The frame every settings page sits in: the way back to the list, the
 * page's name, then the page — or, signed out, a way to sign in.
 */
export function SettingsLayout({ to, children }: { to: string; children: (token: string) => ReactNode }) {
  const token = getToken()
  const page = SETTINGS_PAGES.find((p) => p.to === to)

  return (
    <div className="page-frame">
      <TopBar />
      <Link to="/settings" className="text-muted-foreground hover:text-foreground mb-1 inline-flex items-center gap-1 text-sm no-underline">
        <ChevronLeft className="size-4" aria-hidden />
        {t({ id: 'tabs_bar.settings', defaultMessage: 'Settings' })}
      </Link>
      <h1 className="mb-4 text-lg font-bold">{page?.label()}</h1>
      {!token ? <SignIn /> : <div className="space-y-4">{children(token)}</div>}
    </div>
  )
}
