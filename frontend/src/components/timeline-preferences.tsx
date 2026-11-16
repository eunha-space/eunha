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
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import type { MessageDescriptor } from 'react-intl'

// `simple_form.labels.defaults.setting_display_media_*`.
const DISPLAY_MEDIA: [Preferences['display_media'], MessageDescriptor][] = [
  ['default', { id: 'eunha.timeline_preferences.display_media_default', defaultMessage: 'Hide media marked as sensitive' }],
  ['show_all', { id: 'eunha.timeline_preferences.display_media_show_all', defaultMessage: 'Always show media' }],
  ['hide_all', { id: 'eunha.timeline_preferences.display_media_hide_all', defaultMessage: 'Always hide media' }],
]

/**
 * Mastodon's "Group boosts in timelines" (`aggregate_reblogs`), and from its
 * appearance page how media, content warnings and GIFs are shown
 * (`web.display_media`, `web.expand_content_warnings`, `web.auto_play`),
 * which apps read from `GET /api/v1/preferences`.
 */
export function TimelinePreferences({ token }: { token: string }) {
  const [prefs, setPrefs] = useState<Preferences | null>(null)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    getPreferences(token)
      .then((loaded) => {
        setPrefs(loaded)
        applyPreferences(loaded)
      })
      .catch(() => setFailed(true))
  }, [token])

  if (!prefs) return failed ? null : <SectionSkeleton rows={3} />

  const save = async (changes: Parameters<typeof updatePreferences>[1]) => {
    try {
      const saved = await updatePreferences(token, changes)
      setPrefs(saved)
      // Posts shown from here on follow the saved setting.
      applyPreferences(saved)
    } catch {
      toast.error(t({ id: 'eunha.timeline_preferences.could_not_save_the_setting', defaultMessage: 'Could not save the setting' }))
    }
  }

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'eunha.timeline_preferences.timelines_and_posting', defaultMessage: 'Timelines and posting' })}</h2>
      <Label className="text-sm font-normal">
        <Switch
          checked={prefs.default_spoiler_field}
          onCheckedChange={(on) => void save({ default_spoiler_field: on })}
        />
        {t({ id: 'eunha.timeline_preferences.open_the_content_warning_field_when', defaultMessage: 'Open the content warning field when composing' })}
      </Label>
      <div className="space-y-0.5">
        <Label className="text-sm font-normal">
          <Switch
            checked={prefs.aggregate_reblogs}
            onCheckedChange={(on) => void save({ aggregate_reblogs: on })}
          />
          {t({ id: 'mastodon.simple_form.labels.defaults.setting_aggregate_reblogs', defaultMessage: 'Group boosts in timelines' })}
        </Label>
        <p className="text-muted-foreground pl-10 text-xs">
          {t({ id: 'mastodon.simple_form.hints.defaults.setting_aggregate_reblogs', defaultMessage: 'Do not show new boosts for posts that have been recently boosted (only affects newly-received boosts)' })}
        </p>
      </div>
      <Label className="text-sm font-normal">
        <Switch
          checked={prefs.display_own_posts}
          onCheckedChange={(on) => void save({ display_own_posts: on })}
        />
        {t({ id: 'eunha.timeline_preferences.show_my_posts_in_my_home', defaultMessage: 'Show my posts in my home feed' })}
      </Label>
      <Label className="text-sm font-normal">
        <Switch
          checked={prefs.display_own_boosts}
          onCheckedChange={(on) => void save({ display_own_boosts: on })}
        />
        {t({ id: 'eunha.timeline_preferences.show_my_boosts_in_my_home', defaultMessage: 'Show my boosts in my home feed' })}
      </Label>
      <Label className="text-sm font-normal">
        <Switch
          checked={prefs.expand_content_warnings}
          onCheckedChange={(on) => void save({ expand_content_warnings: on })}
        />
        {t({ id: 'mastodon.simple_form.labels.defaults.setting_expand_spoilers', defaultMessage: 'Always expand posts marked with content warnings' })}
      </Label>
      <Label className="text-sm font-normal">
        <Switch checked={prefs.auto_play} onCheckedChange={(on) => void save({ auto_play: on })} />
        {t({ id: 'mastodon.simple_form.labels.defaults.setting_auto_play_gif', defaultMessage: 'Auto-play animated GIFs' })}
      </Label>
      <div className="space-y-1">
        <Label htmlFor="display-media" className="text-sm font-normal">
          {t({ id: 'mastodon.simple_form.labels.defaults.setting_display_media', defaultMessage: 'Media display' })}
        </Label>
        <Select
          items={Object.fromEntries(DISPLAY_MEDIA.map(([value, label]) => [value, t(label)]))}
          value={prefs.display_media}
          onValueChange={(v) => {
            if (typeof v === 'string') {
              void save({ display_media: v as Preferences['display_media'] })
            }
          }}
        >
          <SelectTrigger id="display-media" className="w-full" aria-label={t({ id: 'mastodon.simple_form.labels.defaults.setting_display_media', defaultMessage: 'Media display' })}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {DISPLAY_MEDIA.map(([value, label]) => (
                <SelectItem key={value} value={value}>
                  {t(label)}
                </SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>
      </div>
    </section>
  )
}
