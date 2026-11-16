import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  getPreferences,
  getTimeZones,
  updatePreferences,
  type Preferences,
  type TimeZoneChoice,
} from '../security-api.ts'
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
import { defineMessage } from 'react-intl'

// `simple_form.labels.notification_emails.*`, in the order Mastodon's
// notifications page lists them.
const NOTIFICATION_EMAILS: [string, MessageDescriptor][] = [
  ['follow', defineMessage({ id: 'mastodon.simple_form.labels.notification_emails.follow', defaultMessage: 'Someone followed you' })],
  [
    'follow_request',
    defineMessage({ id: 'mastodon.simple_form.labels.notification_emails.follow_request', defaultMessage: 'Someone requested to follow you' }),
  ],
  ['reblog', defineMessage({ id: 'mastodon.simple_form.labels.notification_emails.reblog', defaultMessage: 'Someone boosted your post' })],
  [
    'favourite',
    defineMessage({ id: 'mastodon.simple_form.labels.notification_emails.favourite', defaultMessage: 'Someone favourited your post' }),
  ],
  ['mention', defineMessage({ id: 'mastodon.simple_form.labels.notification_emails.mention', defaultMessage: 'Someone mentioned you' })],
  ['quote', defineMessage({ id: 'mastodon.simple_form.labels.notification_emails.quote', defaultMessage: 'Someone quoted you' })],
]

/**
 * Mastodon's notification email preferences (`notification_emails.*` and
 * `always_send_emails`), and the time zone from its appearance page
 * (`users.time_zone`), which the times in those mails are written in.
 */
export function MailPreferences({ token }: { token: string }) {
  const [prefs, setPrefs] = useState<Preferences | null>(null)
  const [zones, setZones] = useState<TimeZoneChoice[]>([])
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    getPreferences(token)
      .then(setPrefs)
      .catch(() => setFailed(true))
    getTimeZones(token)
      .then(setZones)
      .catch(() => setFailed(true))
  }, [token])

  if (!prefs || zones.length === 0) return failed ? null : <SectionSkeleton rows={3} />

  const save = async (changes: Parameters<typeof updatePreferences>[1]) => {
    try {
      setPrefs(await updatePreferences(token, changes))
    } catch {
      toast.error(t({ id: 'eunha.mail_preferences.could_not_save_the_setting', defaultMessage: 'Could not save the setting' }))
    }
  }

  // The form selects `current_user.time_zone || Time.zone.tzinfo.name`; a
  // zone stored by its Rails name rather than its IANA one is shown under the
  // entry carrying that name.
  const stored = prefs.time_zone ?? 'Etc/UTC'
  const selected =
    zones.find((z) => z.value === stored)?.value ??
    zones.find((z) => z.label.endsWith(` ${stored}`))?.value ??
    'Etc/UTC'
  const items = Object.fromEntries(zones.map((z) => [z.value, z.label]))

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">{t({ id: 'mastodon.settings.notifications', defaultMessage: 'Email notifications' })}</h2>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold">{t({ id: 'eunha.mail_preferences.email_me_when', defaultMessage: 'Email me when' })}</h3>
        {NOTIFICATION_EMAILS.map(([key, label]) => (
          <Label key={key} className="text-sm font-normal">
            <Switch
              checked={prefs.notification_emails[key] === true}
              onCheckedChange={(on) => void save({ notification_emails: { [key]: on } })}
            />
            {t(label)}
          </Label>
        ))}
      </div>
      <div className="space-y-0.5">
        <Label className="text-sm font-normal">
          <Switch
            checked={prefs.always_send_emails}
            onCheckedChange={(on) => void save({ always_send_emails: on })}
          />
          {t({ id: 'mastodon.simple_form.labels.defaults.setting_always_send_emails', defaultMessage: 'Always send e-mail notifications' })}
        </Label>
        <p className="text-muted-foreground pl-10 text-xs">
          {t({ id: 'mastodon.simple_form.hints.defaults.setting_always_send_emails', defaultMessage: 'Normally e-mail notifications won\'\'t be sent when you are actively using Mastodon' })}
        </p>
      </div>
      <h3 className="text-sm font-semibold">{t({ id: 'mastodon.simple_form.labels.user.time_zone', defaultMessage: 'Time zone' })}</h3>
      <div className="space-y-1">
        <Label htmlFor="time-zone" className="text-sm font-normal">
          {t({ id: 'eunha.mail_preferences.times_in_the_emails_you_receive', defaultMessage: 'Times in the emails you receive are written in this zone.' })}
        </Label>
        <Select
          items={items}
          value={selected}
          onValueChange={(v) => {
            if (typeof v === 'string') void save({ time_zone: v })
          }}
        >
          <SelectTrigger id="time-zone" className="w-full" aria-label={t({ id: 'mastodon.simple_form.labels.user.time_zone', defaultMessage: 'Time zone' })}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {zones.map((z) => (
                <SelectItem key={z.label} value={z.value}>
                  {z.label}
                </SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>
      </div>
    </section>
  )
}
