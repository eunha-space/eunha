import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import { getCurrentAccount } from '../api.ts'
import { restClient } from '../masto.ts'
import { getPreferences, updatePreferences, type Preferences } from '../security-api.ts'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'

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
 * search on the account, through `update_credentials`, and the two user
 * settings it has no API for, through eunha's preferences.
 */
export function PrivacySettings({ token }: { token: string }) {
  const [account, setAccount] = useState<AccountPrivacy | null>(null)
  const [prefs, setPrefs] = useState<Preferences | null>(null)

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
      .catch(() => {})
    getPreferences(token)
      .then(setPrefs)
      .catch(() => {})
  }, [token])

  if (!account || !prefs) return null

  const saveAccount = async (changes: Partial<AccountPrivacy>) => {
    const previous = account
    setAccount({ ...account, ...changes })
    try {
      await restClient(token).v1.accounts.updateCredentials(changes)
    } catch {
      setAccount(previous)
      toast.error('Could not save the setting')
    }
  }

  const savePrefs = async (changes: Parameters<typeof updatePreferences>[1]) => {
    try {
      setPrefs(await updatePreferences(token, changes))
    } catch {
      toast.error('Could not save the setting')
    }
  }

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">Privacy and reach</h2>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">Reach</h3>
        <Toggle
          label="Feature profile and posts in discovery algorithms"
          checked={account.discoverable}
          onChange={(on) => void saveAccount({ discoverable: on })}
        />
        <Toggle
          label="Automatically accept new followers"
          checked={!account.locked}
          onChange={(on) => void saveAccount({ locked: !on })}
        />
        <Toggle
          label="Show who you follow and who follows you on your profile"
          checked={!account.hideCollections}
          onChange={(on) => void saveAccount({ hideCollections: !on })}
        />
      </div>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">Search</h3>
        <Toggle
          label="Include public posts in search results"
          checked={account.indexable}
          onChange={(on) => void saveAccount({ indexable: on })}
        />
        <Toggle
          label="Include profile page in search engines"
          hint="When off, search engines such as Google are asked not to index your profile."
          checked={!prefs.noindex}
          onChange={(on) => void savePrefs({ noindex: !on })}
        />
      </div>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">Privacy</h3>
        <Toggle
          label="Display from which app you sent a post"
          checked={prefs.show_application}
          onChange={(on) => void savePrefs({ show_application: on })}
        />
      </div>
    </section>
  )
}
