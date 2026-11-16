import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { ChevronLeft, ChevronRight } from 'lucide-react'

import { beginLogin, getToken } from '../auth.ts'
import { getMeAccount } from '../me.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { t } from '@/i18n.tsx'

/**
 * The settings pages, at Mastodon's paths for them and in the order its
 * `config/navigation.rb` lists them, so a link Mastodon or an app makes to one
 * lands on the matching page. `/settings` itself lists them.
 */
export const SETTINGS_PAGES: { to: string; label: () => string; summary: () => string }[] = [
  {
    to: '/settings/account',
    label: () => t({ id: 'navigation_bar.account_settings', defaultMessage: 'Password and security' }),
    summary: () => t({ id: 'eunha.settings_layout.account_summary', defaultMessage: 'Email, password, two-factor, sessions' }),
  },
  {
    to: '/settings/privacy',
    label: () => t({ id: 'navigation_bar.privacy_and_reach', defaultMessage: 'Privacy and reach' }),
    summary: () => t({ id: 'eunha.settings_layout.privacy_summary', defaultMessage: 'Discovery, blocks, filters' }),
  },
  {
    to: '/settings/preferences/appearance',
    label: () => t({ id: 'mastodon.settings.appearance', defaultMessage: 'Appearance' }),
    summary: () => t({ id: 'eunha.settings_layout.appearance_summary', defaultMessage: 'Palette, layout, timelines' }),
  },
  {
    to: '/settings/preferences/posting_defaults',
    label: () => t({ id: 'mastodon.preferences.posting_defaults', defaultMessage: 'Posting defaults' }),
    summary: () => t({ id: 'eunha.settings_layout.posting_summary', defaultMessage: 'Language, quoting, collections' }),
  },
  {
    to: '/settings/preferences/notifications',
    label: () => t({ id: 'column.notifications', defaultMessage: 'Notifications' }),
    summary: () => t({ id: 'eunha.settings_layout.notifications_summary', defaultMessage: 'Email and browser' }),
  },
  {
    to: '/settings/statuses_cleanup',
    label: () => t({ id: 'navigation_bar.automated_deletion', defaultMessage: 'Automated post deletion' }),
    summary: () => t({ id: 'eunha.settings_layout.statuses_cleanup_summary', defaultMessage: 'Delete old posts on a schedule' }),
  },
  {
    to: '/settings/export',
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
          <ul className="divide-y rounded-lg border">
            {SETTINGS_PAGES.map((page) => (
              <li key={page.to}>
                <Link
                  to={page.to}
                  className="hover:bg-muted/60 flex items-center gap-3 px-4 py-3 text-inherit no-underline"
                >
                  <span className="min-w-0 flex-1">
                    <span className="block font-medium">{page.label()}</span>
                    <span className="text-muted-foreground block text-sm">{page.summary()}</span>
                  </span>
                  <ChevronRight className="text-muted-foreground size-4 shrink-0" aria-hidden />
                </Link>
              </li>
            ))}
          </ul>
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
