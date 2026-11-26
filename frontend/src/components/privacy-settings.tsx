import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import { getCurrentAccount } from '../api.ts'
import { restClient } from '../masto.ts'
import {
  getPreferences,
  getRelayPreferences,
  updatePreferences,
  updateRelayPreferences,
  type OptInRelay,
  type Preferences,
} from '../security-api.ts'
import { EmojiHtml } from '@/components/emoji.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

interface AccountPrivacy {
  discoverable: boolean
  locked: boolean
  indexable: boolean
  hideCollections: boolean
}

function Toggle({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string
  hint?: string
  checked: boolean
  onChange: (on: boolean) => void
}) {
  return (
    <div className="space-y-0.5">
      <Label className="text-sm font-normal">
        <Switch checked={checked} onCheckedChange={onChange} />
        {label}
      </Label>
      {hint && <p className="text-muted-foreground pl-10 text-xs">{hint}</p>}
    </div>
  )
}

/**
 * Mastodon's privacy settings page (`Settings::PrivacyController`): reach and
 * search on the account, through `update_credentials`, the two user settings
 * it has no API for, through eunha's preferences, and the relays that need
 * members to opt in (`Settings::RelayPreferencesController`), when there are
 * any.
 */
export function PrivacySettings({ token }: { token: string }) {
  const [account, setAccount] = useState<AccountPrivacy | null>(null)
  const [prefs, setPrefs] = useState<Preferences | null>(null)
  const [relays, setRelays] = useState<OptInRelay[]>([])
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    getCurrentAccount(token)
      .then((me) => {
        // masto types `indexable` and `hide_collections` only on `source`.
        const extra = me as unknown as { indexable?: boolean; hideCollections?: boolean | null }
        setAccount({
          discoverable: me.discoverable ?? false,
          locked: me.locked,
          indexable: extra.indexable ?? false,
          hideCollections: extra.hideCollections ?? false,
        })
      })
      .catch(() => setFailed(true))
    getPreferences(token)
      .then(setPrefs)
      .catch(() => setFailed(true))
    getRelayPreferences(token)
      .then(setRelays)
      .catch(() => {})
  }, [token])

  if (!account || !prefs) return failed ? null : <SectionSkeleton rows={4} />

  const saveAccount = async (changes: Partial<AccountPrivacy>) => {
    const previous = account
    setAccount({ ...account, ...changes })
    try {
      await restClient(token).v1.accounts.updateCredentials(changes)
    } catch {
      setAccount(previous)
      toast.error(t({ id: 'eunha.privacy_settings.could_not_save_the_setting', defaultMessage: 'Could not save the setting' }))
    }
  }

  const savePrefs = async (changes: Parameters<typeof updatePreferences>[1]) => {
    try {
      setPrefs(await updatePreferences(token, changes))
    } catch {
      toast.error(t({ id: 'eunha.privacy_settings.could_not_save_the_setting', defaultMessage: 'Could not save the setting' }))
    }
  }

  const saveRelay = async (relay: OptInRelay, on: boolean) => {
    const ids = relays
      .filter((r) => (r.id === relay.id ? on : r.enabled))
      .map((r) => r.id)
    try {
      setRelays(await updateRelayPreferences(token, ids))
    } catch {
      toast.error(t({ id: 'eunha.privacy_settings.could_not_save_the_setting', defaultMessage: 'Could not save the setting' }))
    }
  }

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'eunha.navigation_bar.privacy_and_reach', defaultMessage: 'Privacy and reach' })}</h2>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">{t({ id: 'mastodon.privacy.reach', defaultMessage: 'Reach' })}</h3>
        <Toggle
          label={t({ id: 'eunha.privacy_settings.feature_profile_and_posts_in_discovery', defaultMessage: 'Feature profile and posts in discovery algorithms' })}
          checked={account.discoverable}
          onChange={(on) => void saveAccount({ discoverable: on })}
        />
        <Toggle
          label={t({ id: 'mastodon.simple_form.labels.account.unlocked', defaultMessage: 'Automatically accept new followers' })}
          checked={!account.locked}
          onChange={(on) => void saveAccount({ locked: !on })}
        />
        <Toggle
          label={t({ id: 'eunha.privacy_settings.show_who_you_follow_and_who', defaultMessage: 'Show who you follow and who follows you on your profile' })}
          checked={!account.hideCollections}
          onChange={(on) => void saveAccount({ hideCollections: !on })}
        />
      </div>
      {relays.length > 0 && (
        <div className="space-y-2">
          <h3 className="text-sm font-semibold">{t({ id: 'mastodon.relay_preferences.title', defaultMessage: 'Relay preferences' })}</h3>
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.privacy_settings.relays_hint', defaultMessage: 'Relays you opt in to are sent your public posts, which they share further to a wider audience. Other kinds of posts are never sent to relays.' })}
          </p>
          {relays.map((relay) => (
            <div key={relay.id} className="space-y-0.5">
              <Label className="text-sm font-normal">
                <Switch checked={relay.enabled} onCheckedChange={(on) => void saveRelay(relay, on)} />
                <bdi className="font-medium">{relay.display_name || relay.uri}</bdi>
              </Label>
              {relay.note && (
                <EmojiHtml html={relay.note} emojis={[]} className="text-muted-foreground pl-10 text-xs" />
              )}
            </div>
          ))}
        </div>
      )}
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">{t({ id: 'navigation_bar.search', defaultMessage: 'Search' })}</h3>
        <Toggle
          label={t({ id: 'mastodon.simple_form.labels.account.indexable', defaultMessage: 'Include public posts in search results' })}
          checked={account.indexable}
          onChange={(on) => void saveAccount({ indexable: on })}
        />
        <Toggle
          label={t({ id: 'mastodon.simple_form.labels.settings.indexable', defaultMessage: 'Include profile page in search engines' })}
          hint={t({ id: 'eunha.privacy_settings.when_off_search_engines_such_as', defaultMessage: 'When off, search engines such as Google are asked not to index your profile.' })}
          checked={!prefs.noindex}
          onChange={(on) => void savePrefs({ noindex: !on })}
        />
      </div>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">{t({ id: 'footer.privacy_policy_short', defaultMessage: 'Privacy' })}</h3>
        <Toggle
          label={t({ id: 'mastodon.simple_form.labels.settings.show_application', defaultMessage: 'Display from which app you sent a post' })}
          checked={prefs.show_application}
          onChange={(on) => void savePrefs({ show_application: on })}
        />
      </div>
    </section>
  )
}
