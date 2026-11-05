import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  getStatusesCleanup,
  updateStatusesCleanup,
  type StatusesCleanupPolicy,
} from '../security-api.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'
import { Switch } from '@/components/ui/switch.tsx'

// `statuses_cleanup.min_age`, by `ALLOWED_MIN_STATUS_AGE`.
const AGES = [
  { value: 604800, label: '1 week' },
  { value: 1209600, label: '2 weeks' },
  { value: 2629746, label: '1 month' },
  { value: 5259492, label: '2 months' },
  { value: 7889238, label: '3 months' },
  { value: 15778476, label: '6 months' },
  { value: 31556952, label: '1 year' },
  { value: 63113904, label: '2 years' },
]

type KeepField =
  | 'keep_pinned'
  | 'keep_direct'
  | 'keep_self_fav'
  | 'keep_self_bookmark'
  | 'keep_polls'
  | 'keep_media'

const EXCEPTIONS: [KeepField, string][] = [
  ['keep_pinned', 'Keep pinned posts'],
  ['keep_direct', 'Keep direct messages'],
  ['keep_self_fav', 'Keep posts you favorited'],
  ['keep_self_bookmark', 'Keep posts you bookmarked'],
  ['keep_polls', 'Keep polls'],
  ['keep_media', 'Keep posts with media attachments'],
]

/**
 * Mastodon's automated post deletion page (`StatusesCleanupController`). It
 * saves as one form rather than a switch at a time, so that turning it on never
 * deletes anything by a choice not yet finished.
 */
export function StatusesCleanupSettings({ token }: { token: string }) {
  const [policy, setPolicy] = useState<StatusesCleanupPolicy | null>(null)
  const [minFavs, setMinFavs] = useState('')
  const [minReblogs, setMinReblogs] = useState('')
  const [busy, setBusy] = useState(false)

  const load = (saved: StatusesCleanupPolicy) => {
    setPolicy(saved)
    setMinFavs(saved.min_favs?.toString() ?? '')
    setMinReblogs(saved.min_reblogs?.toString() ?? '')
  }

  useEffect(() => {
    getStatusesCleanup(token)
      .then(load)
      .catch(() => {})
  }, [token])

  if (!policy) return null

  const change = (changes: Partial<StatusesCleanupPolicy>) =>
    setPolicy({ ...policy, ...changes })

  const save = async (event: FormEvent) => {
    event.preventDefault()
    setBusy(true)
    try {
      load(
        await updateStatusesCleanup(token, {
          ...policy,
          min_favs: minFavs.trim() === '' ? null : Number(minFavs),
          min_reblogs: minReblogs.trim() === '' ? null : Number(minReblogs),
        }),
      )
      toast.success('Changes saved')
    } catch (error) {
      toast.error(error instanceof Error ? error.message : 'Could not save the changes')
    } finally {
      setBusy(false)
    }
  }

  const off = !policy.enabled

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <h2 className="font-semibold">Automated post deletion</h2>
      <form className="space-y-4" onSubmit={(event) => void save(event)}>
        <div className="space-y-0.5">
          <Label className="text-sm font-normal">
            <Switch
              checked={policy.enabled}
              onCheckedChange={(enabled) => change({ enabled })}
            />
            Automatically delete old posts
          </Label>
          <p className="text-muted-foreground pl-10 text-xs">
            Deletes your posts once they reach the age below, unless they match one of
            the exceptions. Deleted posts cannot be brought back. This runs at low
            priority, so a post may stay a while after reaching that age.
          </p>
        </div>

        <div className="space-y-1">
          <Label htmlFor="cleanup-age" className="text-sm font-normal">
            Age threshold
          </Label>
          <Select
            items={AGES.map(({ value, label }) => ({ value: String(value), label }))}
            value={String(policy.min_status_age)}
            disabled={off}
            onValueChange={(value) => {
              if (typeof value === 'string') change({ min_status_age: Number(value) })
            }}
          >
            <SelectTrigger id="cleanup-age" className="w-full" aria-label="Age threshold">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectGroup>
                {AGES.map(({ value, label }) => (
                  <SelectItem key={value} value={String(value)}>
                    {label}
                  </SelectItem>
                ))}
              </SelectGroup>
            </SelectContent>
          </Select>
        </div>

        <div className="space-y-2">
          <h3 className="text-sm font-semibold">Exceptions</h3>
          {EXCEPTIONS.map(([field, label]) => (
            <Label key={field} className="text-sm font-normal">
              <Switch
                checked={policy[field]}
                disabled={off}
                onCheckedChange={(on) => change({ [field]: on })}
              />
              {label}
            </Label>
          ))}
        </div>

        <div className="space-y-2">
          <h3 className="text-sm font-semibold">Exceptions based on interactions</h3>
          <div className="space-y-1">
            <Label htmlFor="cleanup-favs" className="text-sm font-normal">
              Keep posts favorited at least
            </Label>
            <Input
              id="cleanup-favs"
              type="number"
              min={1}
              placeholder="Ignore favorites"
              value={minFavs}
              disabled={off}
              onChange={(event) => setMinFavs(event.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="cleanup-reblogs" className="text-sm font-normal">
              Keep posts boosted at least
            </Label>
            <Input
              id="cleanup-reblogs"
              type="number"
              min={1}
              placeholder="Ignore boosts"
              value={minReblogs}
              disabled={off}
              onChange={(event) => setMinReblogs(event.target.value)}
            />
          </div>
          <p className="text-muted-foreground text-xs">
            Leave blank to delete posts whatever their number of favorites or boosts. A
            post that briefly passes the number may be kept even if it later falls below.
          </p>
        </div>

        <Button type="submit" size="sm" disabled={busy}>
          Save changes
        </Button>
      </form>
    </section>
  )
}
