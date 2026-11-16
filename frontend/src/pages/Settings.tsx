import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { ArrowLeftRight, Ban, KeyRound } from 'lucide-react'
import { toast } from 'sonner'

import {
  ApiError,
  deleteAccount,
  getOwnEmailSubscriptions,
  setOwnEmailSubscriptions,
  type OwnEmailSubscriptions,
} from '../eunha-api.ts'
import {
  getCurrentAccount,
  setDefaultPostLanguage,
  setDefaultQuotePolicy,
  type QuotePolicy,
} from '../api.ts'
import { BrowserPushSettings } from '@/components/browser-push-settings.tsx'
import { PostLanguagePicker } from '@/components/post-language-picker.tsx'
import { beginLogin, getToken, logout } from '../auth.ts'
import { isAdvancedLayout, setAdvancedLayout } from '../lib/panes.ts'
import { clearMe, getMeAccount, loadMe } from '../me.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { EmailSettings } from '@/components/email-settings.tsx'
import { MailPreferences } from '@/components/mail-preferences.tsx'
import { PrivacySettings } from '@/components/privacy-settings.tsx'
import { StatusesCleanupSettings } from '@/components/statuses-cleanup-settings.tsx'
import { SessionsSettings } from '@/components/sessions-settings.tsx'
import { PALETTES, useTheme, type Palette } from '@/components/theme-provider.tsx'
import {
  Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select.tsx'
import { TimelinePreferences } from '@/components/timeline-preferences.tsx'
import { TwoFactorSettings } from '@/components/two-factor-settings.tsx'
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
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'
import type { MessageDescriptor } from 'react-intl'

// Mastodon's `deletes.warning.*`, shown before the challenge.
const WARNINGS: MessageDescriptor[] = [
  { id: 'mastodon.deletes.warning.irreversible', defaultMessage: 'You will not be able to restore or reactivate your account' },
  { id: 'mastodon.deletes.warning.username_unavailable', defaultMessage: 'Your username will remain unavailable' },
  { id: 'mastodon.deletes.warning.data_removal', defaultMessage: 'Your posts and other data will be permanently removed' },
  { id: 'mastodon.deletes.warning.caches', defaultMessage: 'Content that has been cached by other servers may persist' },
]

function DeleteAccount({ token }: { token: string }) {
  // A local account's acct is its username, which the challenge asks for.
  const username = getMeAccount()?.acct ?? ''
  const [typedUsername, setTypedUsername] = useState('')
  const [password, setPassword] = useState('')
  const [confirming, setConfirming] = useState(false)
  const [deleting, setDeleting] = useState(false)

  const submit = async () => {
    setDeleting(true)
    try {
      await deleteAccount(token, { username: typedUsername, password })
      // The account is marked deleted the moment that returns, so this token
      // is already dead — drop the local session rather than let the next
      // request fail on its own.
      await logout()
      clearMe()
      setConfirming(false)
      toast.success(t({ id: 'mastodon.deletes.success_msg', defaultMessage: 'Your account was successfully deleted' }))
      window.location.assign('/')
    } catch (e) {
      setConfirming(false)
      toast.error(
        e instanceof ApiError && e.status === 401
          ? t({ id: 'mastodon.deletes.challenge_not_passed', defaultMessage: 'The information you entered was not correct' })
          : t({ id: 'eunha.settings.could_not_delete_your_account', defaultMessage: 'Could not delete your account' }),
      )
    } finally {
      setDeleting(false)
    }
  }

  return (
    <section className="border-destructive/40 space-y-3 rounded-lg border p-4">
      <div>
        <h2 className="text-destructive font-semibold">{t({ id: 'mastodon.auth.delete_account', defaultMessage: 'Delete account' })}</h2>
        <p className="text-muted-foreground text-sm">
          {t({ id: 'mastodon.deletes.warning.before', defaultMessage: 'Before proceeding, please read these notes carefully:' })}
        </p>
      </div>
      <ul className="text-muted-foreground list-disc space-y-1 pl-5 text-sm">
        {WARNINGS.map((warning) => (
          <li key={warning.id}>{t(warning)}</li>
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
          <Label htmlFor="delete-username">{t({ id: 'eunha.settings.confirm_username', defaultMessage: 'Confirm username' })}</Label>
          <Input
            id="delete-username"
            autoComplete="off"
            value={typedUsername}
            onChange={(e) => setTypedUsername(e.target.value)}
            required
          />
          <p className="text-muted-foreground text-xs">
            <FormattedMessage
              id='mastodon.deletes.confirm_username_hint_html'
              defaultMessage='Please type <code>{username}</code> to confirm the account you want to delete'
              values={{ username, code: (chunks) => <code>{chunks}</code> }}
            />
          </p>
        </div>
        <div className="space-y-1">
          <Label htmlFor="delete-password">{t({ id: 'mastodon.simple_form.labels.defaults.current_password', defaultMessage: 'Current password' })}</Label>
          <Input
            id="delete-password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.settings.enter_your_current_password_to_verify', defaultMessage: 'Enter your current password to verify your identity.' })}
          </p>
        </div>
        <Button
          type="submit"
          variant="destructive"
          disabled={!typedUsername.trim() || !password}
        >
          {t({ id: 'mastodon.auth.delete_account', defaultMessage: 'Delete account' })}
        </Button>
      </form>

      <AlertDialog open={confirming} onOpenChange={setConfirming}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t({ id: 'eunha.settings.delete_your_account', defaultMessage: 'Delete your account?' })}</AlertDialogTitle>
            <AlertDialogDescription>
              {t({ id: 'eunha.settings.this_cannot_be_undone_your_posts', defaultMessage: 'This cannot be undone. Your posts and other data are removed, and your username stays unavailable.' })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={deleting}>{t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              disabled={deleting}
              onClick={submit}
            >
              {deleting ? t({ id: 'eunha.common.deleting', defaultMessage: 'Deleting…' }) : t({ id: 'mastodon.auth.delete_account', defaultMessage: 'Delete account' })}
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
      toast.error(t({ id: 'eunha.settings.could_not_save_the_setting', defaultMessage: 'Could not save the setting' }))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'mastodon.privacy.email_subscriptions', defaultMessage: 'Send posts via email' })}</h2>
      <p className="text-muted-foreground text-sm">
        {t({ id: 'eunha.settings.add_an_email_sign_up_form', defaultMessage: 'Add an email sign-up form to your profile that appears for logged-out users. When visitors enter their email address and opt in, they get email updates for your public posts.' })}
      </p>
      {(own.subscribers > 0 || own.enabled) && (
        <p className="text-sm">
          {own.enabled ? t({ id: 'mastodon.email_subscriptions.active', defaultMessage: 'Active' }) : t({ id: 'mastodon.email_subscriptions.inactive', defaultMessage: 'Inactive' })} · {own.subscribers}{' '}
          {own.subscribers === 1 ? 'subscriber' : 'subscribers'}
        </p>
      )}
      <Label className="text-sm font-normal">
        <Switch checked={own.enabled} disabled={saving} onCheckedChange={toggle} />
        {t({ id: 'eunha.settings.offer_email_subscriptions_on_my_profile', defaultMessage: 'Offer email subscriptions on my profile' })}
      </Label>
    </section>
  )
}

function PostingLanguageSettings({ token }: { token: string }) {
  const [language, setLanguage] = useState<string>()
  const [failed, setFailed] = useState(false)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    let active = true
    getCurrentAccount(token).then((account) => {
      if (active) setLanguage(account.source.language ?? '')
    }).catch(() => setFailed(true))
    return () => { active = false }
  }, [token])

  if (language === undefined) return failed ? null : <SectionSkeleton rows={1} />

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'eunha.settings.default_post_language', defaultMessage: 'Default post language' })}</h2>
      <PostLanguagePicker label={t({ id: 'eunha.settings.default_post_language', defaultMessage: 'Default post language' })} allowDefault value={language} disabled={saving}
        onChange={async (value) => {
          setSaving(true)
          try {
            const account = await setDefaultPostLanguage(token, value)
            setLanguage(account.source.language ?? '')
          } catch {
            toast.error(t({ id: 'eunha.settings.could_not_save_the_default_post', defaultMessage: 'Could not save the default post language' }))
          } finally {
            setSaving(false)
          }
        }} />
    </section>
  )
}

// `statuses.quote_policies.*`, in the order Mastodon's `UserSettings` lists
// `default_quote_policy`'s values.
const QUOTE_POLICIES: [QuotePolicy, MessageDescriptor][] = [
  ['public', { id: 'mastodon.statuses.quote_policies.public', defaultMessage: 'Anyone' }],
  ['followers', { id: 'mastodon.statuses.quote_policies.followers', defaultMessage: 'Followers only' }],
  ['nobody', { id: 'mastodon.statuses.quote_policies.nobody', defaultMessage: 'Just me' }],
]

/**
 * Mastodon's "Who can quote" on its posting defaults page. As there, it is
 * disabled while posts default to followers-only, which no one else may
 * quote, and a quiet public default carries its hint
 * (`simple_form.hints.defaults.setting_default_quote_policy_*`).
 */
function QuotePolicySettings({ token }: { token: string }) {
  const [account, setAccount] = useState<{ policy: QuotePolicy; privacy: string }>()
  const [failed, setFailed] = useState(false)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    let active = true
    getCurrentAccount(token).then((me) => {
      if (active) setAccount({ policy: quotePolicyOf(me), privacy: me.source.privacy ?? 'public' })
    }).catch(() => setFailed(true))
    return () => { active = false }
  }, [token])

  if (!account) return failed ? null : <SectionSkeleton rows={1} />

  const hint = account.privacy === 'private'
    ? t({ id: 'mastodon.simple_form.hints.defaults.setting_default_quote_policy_private', defaultMessage: 'Followers-only posts authored on Mastodon can\'\'t be quoted by others.' })
    : account.privacy === 'unlisted'
      ? t({ id: 'mastodon.simple_form.hints.defaults.setting_default_quote_policy_unlisted', defaultMessage: 'When people quote you, their post will also be hidden from trending timelines.' })
      : null

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'compose.visibility.quote_policy', defaultMessage: 'Who can quote' })}</h2>
      <Select
        items={QUOTE_POLICIES.map(([value, label]) => ({ value, label: t(label) }))}
        value={account.policy}
        disabled={saving || account.privacy === 'private'}
        onValueChange={async (value) => {
          if (typeof value !== 'string') return
          setSaving(true)
          try {
            const me = await setDefaultQuotePolicy(token, value as QuotePolicy)
            setAccount({ policy: quotePolicyOf(me), privacy: me.source.privacy ?? 'public' })
            // The composer reads the default from the cached account.
            await loadMe(token)
          } catch {
            toast.error(t({ id: 'eunha.settings.could_not_save_who_can_quote', defaultMessage: 'Could not save who can quote' }))
          } finally {
            setSaving(false)
          }
        }}
      >
        <SelectTrigger className="w-full" aria-label={t({ id: 'compose.visibility.quote_policy', defaultMessage: 'Who can quote' })}>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectGroup>
            {QUOTE_POLICIES.map(([value, label]) => (
              <SelectItem key={value} value={value}>{t(label)}</SelectItem>
            ))}
          </SelectGroup>
        </SelectContent>
      </Select>
      {hint && <p className="text-muted-foreground text-xs">{hint}</p>}
    </section>
  )
}

function quotePolicyOf(me: { source: object }): QuotePolicy {
  const policy = (me.source as { quotePolicy?: unknown }).quotePolicy
  return policy === 'followers' || policy === 'nobody' ? policy : 'public'
}

export default function Settings() {
  const { palette, setPalette } = useTheme()
  const token = getToken()
  const [advanced, setAdvanced] = useState(() => isAdvancedLayout())
  const me = getMeAccount()

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-1 text-lg font-bold">{t({ id: 'tabs_bar.settings', defaultMessage: 'Settings' })}</h1>
      <p className="text-muted-foreground mb-4 text-sm">
        {me ? t({ id: 'eunha.settings.signed_in_as', defaultMessage: 'Signed in as @{acct}' }, { acct: me.acct }) : t({ id: 'eunha.settings.your_account', defaultMessage: 'Your account' })}
      </p>

      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.settings.sign_in_to_manage_your_account', defaultMessage: 'Sign in to manage your account.' })}
          </p>
          <Button size="sm" onClick={() => beginLogin()}>
            {t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}
          </Button>
        </div>
      ) : (
        <div className="space-y-4">
          {/* Password changes still live on the server-rendered account pages. */}
          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'mastodon.activerecord.attributes.user.password', defaultMessage: 'Password' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.settings.change_your_password_on_the_account', defaultMessage: 'Change your password on the account page.' })}
            </p>
            <Button variant="secondary" size="sm" render={<a href="/account/password" />}>
              <KeyRound /> {t({ id: 'mastodon.devise.mailer.reset_password_instructions.action', defaultMessage: 'Change password' })}
            </Button>
          </section>

          <EmailSettings token={token} />

          <TwoFactorSettings token={token} />

          <SessionsSettings token={token} />

          <PrivacySettings token={token} />

          <StatusesCleanupSettings token={token} />

          <MailPreferences token={token} />

          <BrowserPushSettings token={token} />

          <TimelinePreferences token={token} />

          <PostingLanguageSettings token={token} />

          <QuotePolicySettings token={token} />

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'navigation_bar.collections', defaultMessage: 'Collections' })}</h2>
            <Button variant="outline" size="sm" render={<Link to="/collections" />}>{t({ id: 'eunha.settings.manage_collections', defaultMessage: 'Manage collections' })}</Button>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'eunha.settings.content_filters', defaultMessage: 'Content filters' })}</h2>
            <Button variant="outline" size="sm" render={<Link to="/filters" />}>{t({ id: 'eunha.settings.manage_filters', defaultMessage: 'Manage filters' })}</Button>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'mastodon.settings.appearance', defaultMessage: 'Appearance' })}</h2>
            <Label htmlFor="color-palette">{t({ id: 'eunha.settings.color_palette', defaultMessage: 'Color palette' })}</Label>
            <Select
              items={PALETTES}
              value={palette}
              onValueChange={(value) => { if (value) setPalette(value as Palette) }}
            >
              <SelectTrigger id="color-palette"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  {Object.entries(PALETTES).map(([value, label]) => (
                    <SelectItem key={value} value={value}>{label}</SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'eunha.settings.layout', defaultMessage: 'Layout' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.settings.show_several_timelines_side_by_side', defaultMessage: 'Show several timelines side by side instead of one column. Stored in this browser, and only used on a wide one — a narrow screen stays on the single column whatever this says.' })}
            </p>
            <Label className="text-sm font-normal">
              <Switch
                checked={advanced}
                onCheckedChange={(on) => {
                  setAdvancedLayout(on)
                  setAdvanced(on)
                }}
              />
              {t({ id: 'eunha.settings.advanced_layout', defaultMessage: 'Advanced layout' })}
            </Label>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'eunha.settings.blocked_and_muted', defaultMessage: 'Blocked and muted' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.settings.review_the_accounts_you_have_blocked', defaultMessage: 'Review the accounts you have blocked or muted, and undo either.' })}
            </p>
            <Button variant="secondary" size="sm" render={<Link to="/blocked" />}>
              <Ban /> {t({ id: 'eunha.settings.blocked_and_muted', defaultMessage: 'Blocked and muted' })}
            </Button>
          </section>

          <section className="space-y-2 rounded-lg border p-4">
            <h2 className="font-semibold">{t({ id: 'navigation_bar.import_export', defaultMessage: 'Import and export' })}</h2>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.settings.download_your_follows_lists_blocks_and', defaultMessage: 'Download your follows, lists, blocks and the rest, or an archive of your posts and media; bring lists in from another server.' })}
            </p>
            <Button variant="secondary" size="sm" render={<Link to="/settings/export" />}>
              <ArrowLeftRight /> {t({ id: 'navigation_bar.import_export', defaultMessage: 'Import and export' })}
            </Button>
          </section>

          <EmailSubscriptions token={token} />

          <DeleteAccount token={token} />
        </div>
      )}
    </div>
  )
}
