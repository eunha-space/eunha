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
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'

/**
 * The time zone from Mastodon's appearance page (`users.time_zone`), which
 * is what the times in mail are written in.
 */
export function MailPreferences({ token }: { token: string }) {
  const [prefs, setPrefs] = useState<Preferences | null>(null)
  const [zones, setZones] = useState<TimeZoneChoice[]>([])

  useEffect(() => {
    getPreferences(token)
      .then(setPrefs)
      .catch(() => {})
    getTimeZones(token)
      .then(setZones)
      .catch(() => {})
  }, [token])

  if (!prefs || zones.length === 0) return null

  const save = async (changes: Parameters<typeof updatePreferences>[1]) => {
    try {
      setPrefs(await updatePreferences(token, changes))
    } catch {
      toast.error('Could not save the setting')
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
      <h2 className="font-semibold">Time zone</h2>
      <div className="space-y-1">
        <Label htmlFor="time-zone" className="text-sm font-normal">
          Times in the emails you receive are written in this zone.
        </Label>
        <Select
          items={items}
          value={selected}
          onValueChange={(v) => {
            if (typeof v === 'string') void save({ time_zone: v })
          }}
        >
          <SelectTrigger id="time-zone" className="w-full" aria-label="Time zone">
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
