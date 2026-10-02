import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import { getPreferences, updatePreferences, type Preferences } from '../security-api.ts'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'

/** Mastodon's "Group boosts in timelines" (`aggregate_reblogs`). */
export function TimelinePreferences({ token }: { token: string }) {
  const [prefs, setPrefs] = useState<Preferences | null>(null)

  useEffect(() => {
    getPreferences(token)
      .then(setPrefs)
      .catch(() => {})
  }, [token])

  if (!prefs) return null

  const toggle = async (on: boolean) => {
    try {
      setPrefs(await updatePreferences(token, { aggregate_reblogs: on }))
    } catch {
      toast.error('Could not save the setting')
    }
  }

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Timelines</h2>
      <div className="space-y-0.5">
        <Label className="text-sm font-normal">
          <Switch checked={prefs.aggregate_reblogs} onCheckedChange={(on) => void toggle(on)} />
          Group boosts in timelines
        </Label>
        <p className="text-muted-foreground pl-10 text-xs">
          Do not show new boosts for posts that have been recently boosted (only affects
          newly-received boosts)
        </p>
      </div>
    </section>
  )
}
