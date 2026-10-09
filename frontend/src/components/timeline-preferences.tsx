import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import { getPreferences, updatePreferences, type Preferences } from '../security-api.ts'
import { applyPreferences } from '../reading-preferences.ts'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'

// `simple_form.labels.defaults.setting_display_media_*`.
const DISPLAY_MEDIA: [Preferences['display_media'], string][] = [
  ['default', 'Hide media marked as sensitive'],
  ['show_all', 'Always show media'],
  ['hide_all', 'Always hide media'],
]

/**
 * Mastodon's "Group boosts in timelines" (`aggregate_reblogs`), and from its
 * appearance page how media, content warnings and GIFs are shown
 * (`web.display_media`, `web.expand_content_warnings`, `web.auto_play`),
 * which apps read from `GET /api/v1/preferences`.
 */
export function TimelinePreferences({ token }: { token: string }) {
  const [prefs, setPrefs] = useState<Preferences | null>(null)

  useEffect(() => {
    getPreferences(token)
      .then((loaded) => {
        setPrefs(loaded)
        applyPreferences(loaded)
      })
      .catch(() => {})
  }, [token])

  if (!prefs) return null

  const save = async (changes: Parameters<typeof updatePreferences>[1]) => {
    try {
      const saved = await updatePreferences(token, changes)
      setPrefs(saved)
      // Posts shown from here on follow the saved setting.
      applyPreferences(saved)
    } catch {
      toast.error('Could not save the setting')
    }
  }

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Timelines</h2>
      <div className="space-y-0.5">
        <Label className="text-sm font-normal">
          <Switch
            checked={prefs.aggregate_reblogs}
            onCheckedChange={(on) => void save({ aggregate_reblogs: on })}
          />
          Group boosts in timelines
        </Label>
        <p className="text-muted-foreground pl-10 text-xs">
          Do not show new boosts for posts that have been recently boosted (only affects
          newly-received boosts)
        </p>
      </div>
      <Label className="text-sm font-normal">
        <Switch
          checked={prefs.expand_content_warnings}
          onCheckedChange={(on) => void save({ expand_content_warnings: on })}
        />
        Always expand posts marked with content warnings
      </Label>
      <Label className="text-sm font-normal">
        <Switch checked={prefs.auto_play} onCheckedChange={(on) => void save({ auto_play: on })} />
        Auto-play animated GIFs
      </Label>
      <div className="space-y-1">
        <Label htmlFor="display-media" className="text-sm font-normal">
          Media display
        </Label>
        <Select
          items={Object.fromEntries(DISPLAY_MEDIA)}
          value={prefs.display_media}
          onValueChange={(v) => {
            if (typeof v === 'string') {
              void save({ display_media: v as Preferences['display_media'] })
            }
          }}
        >
          <SelectTrigger id="display-media" className="w-full" aria-label="Media display">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {DISPLAY_MEDIA.map(([value, label]) => (
                <SelectItem key={value} value={value}>
                  {label}
                </SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>
      </div>
    </section>
  )
}
