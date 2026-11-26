import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createRelay,
  deleteRelay,
  listRelays,
  setRelayEnabled,
  type AdminRelay,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const STATES: Record<AdminRelay['state'], string> = {
  get idle() { return t({ id: 'mastodon.admin.relays.disabled', defaultMessage: 'Disabled' }) },
  get pending() { return t({ id: 'eunha.admin.relays.waiting_for_relay_s_approval', defaultMessage: 'Waiting for relay’s approval' }) },
  get accepted() { return t({ id: 'mastodon.admin.relays.enabled', defaultMessage: 'Enabled' }) },
  get rejected() { return t({ id: 'eunha.admin.relays.rejected_by_relay', defaultMessage: 'Rejected by relay' }) },
}

/** `simple_form.labels.relay.directions` and their hints. */
const DIRECTIONS: { value: AdminRelay['direction']; label: () => string; hint: () => string }[] = [
  {
    value: 'bidirectional',
    label: () => t({ id: 'mastodon.simple_form.labels.relay.directions.bidirectional', defaultMessage: 'Bidirectional' }),
    hint: () => t({ id: 'mastodon.simple_form.hints.relay.directions.bidirectional', defaultMessage: 'Broadcast to the relay and accept content from other members of the relay' }),
  },
  {
    value: 'broadcast',
    label: () => t({ id: 'mastodon.simple_form.labels.relay.directions.broadcast', defaultMessage: 'Broadcast-only' }),
    hint: () => t({ id: 'mastodon.simple_form.hints.relay.directions.broadcast', defaultMessage: 'Broadcast to the relay only' }),
  },
]

/**
 * Relays: Mastodon's `Admin::RelaysController`. A relay passes public posts
 * between the servers that subscribe to it; a broadcast-only one only takes
 * this server's, and one that requires opt-in only those of members who opt
 * in.
 */
export default function Relays() {
  const token = getToken()
  const [relays, setRelays] = useState<AdminRelay[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [acct, setAcct] = useState('')
  const [direction, setDirection] = useState<AdminRelay['direction']>('bidirectional')
  const [requiresOptIn, setRequiresOptIn] = useState(false)
  const [saving, setSaving] = useState(false)

  const load = () => {
    if (!token) return
    listRelays(token)
      .then((r) => {
        setRelays(r)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  const add = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      await createRelay(token ?? '', { acct, direction, requires_opt_in: requiresOptIn })
      setAcct('')
      setDirection('bidirectional')
      setRequiresOptIn(false)
      toast.success(t({ id: 'eunha.admin.relays.relay_added_it_is_enabled_once', defaultMessage: 'Relay added. It is enabled once the relay approves.' }))
      load()
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const toggle = async (relay: AdminRelay) => {
    try {
      await setRelayEnabled(token ?? '', relay.id, relay.state === 'idle' || relay.state === 'rejected')
      load()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.relays.title', defaultMessage: 'Relays' })} permission="manage_federation">
      <p className="text-muted-foreground mb-3 text-sm">
        {t({ id: 'eunha.admin.relays.a_federation_relay_is_an_intermediary', defaultMessage: 'A federation relay is an intermediary server that exchanges large volumes of public posts between servers that subscribe and publish to it. It can help small and medium servers discover content from the fediverse. Relays do not work while the server requires signed fetches.' })}
      </p>
      <form onSubmit={add} className="mb-4 space-y-3">
        <div className="space-y-1">
          <Label htmlFor="relay-acct">{t({ id: 'mastodon.simple_form.labels.relay.acct', defaultMessage: 'Handle of the relay' })}</Label>
          <Input
            id="relay-acct"
            placeholder="relay@relay.example.com"
            value={acct}
            onChange={(e) => setAcct(e.target.value)}
          />
          <p className="text-muted-foreground text-xs">{t({ id: 'mastodon.simple_form.hints.relay.acct', defaultMessage: 'In the format username@domain.' })}</p>
        </div>
        <div className="space-y-1">
          <p className="text-sm font-medium">{t({ id: 'mastodon.simple_form.labels.relay.direction', defaultMessage: 'Participation level' })}</p>
          <RadioGroup
            value={direction}
            onValueChange={(v) => setDirection(v === 'broadcast' ? 'broadcast' : 'bidirectional')}
          >
            {DIRECTIONS.map((d) => (
              <Label key={d.value} className="items-start text-sm font-normal">
                <RadioGroupItem value={d.value} className="mt-0.5" />
                <span>
                  {d.label()}
                  <span className="text-muted-foreground block text-xs">{d.hint()}</span>
                </span>
              </Label>
            ))}
          </RadioGroup>
        </div>
        <Label className="items-start text-sm font-normal">
          <Checkbox
            checked={requiresOptIn}
            onCheckedChange={(v) => setRequiresOptIn(v === true)}
            className="mt-0.5"
          />
          <span>
            {t({ id: 'mastodon.simple_form.labels.relay.requires_opt_in', defaultMessage: 'Require user opt-in' })}
            <span className="text-muted-foreground block text-xs">{t({ id: 'mastodon.simple_form.hints.relay.requires_opt_in', defaultMessage: 'Only send content to the relay for users who explicitly opt-in to this relay' })}</span>
          </span>
        </Label>
        <Button type="submit" disabled={saving || !acct.trim()}>
          {t({ id: 'mastodon.admin.relays.save_and_enable', defaultMessage: 'Save and enable' })}
        </Button>
      </form>
      <AdminError error={error} />
      {relays === null && !error && <RowsSkeleton action />}
      {relays?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.relays.no_relays_have_been_set_up', defaultMessage: 'No relays have been set up yet.' })}</p>
      )}
      <div className="space-y-2">
        {relays?.map((relay) => (
          <div key={relay.id} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
            <div className="min-w-0 flex-1">
              <p className="truncate text-sm font-medium">{relay.display_name || relay.uri}</p>
              <p className="text-muted-foreground truncate text-xs">
                <samp>{relay.uri}</samp>
                {' · '}
                {relay.direction === 'broadcast'
                  ? t({ id: 'mastodon.admin.relays.broadcast', defaultMessage: 'Broadcast-only' })
                  : t({ id: 'mastodon.admin.relays.bidirectional', defaultMessage: 'Bidirectional' })}
                {' · '}
                {relay.requires_opt_in
                  ? t({ id: 'mastodon.admin.relays.opt_in', defaultMessage: 'Opt-in' })
                  : t({ id: 'mastodon.admin.relays.opt_out', defaultMessage: 'Opt-out' })}
              </p>
            </div>
            <Badge variant={relay.enabled ? 'default' : 'outline'}>{STATES[relay.state]}</Badge>
            <Button size="xs" variant="outline" onClick={() => void toggle(relay)}>
              {relay.state === 'idle' || relay.state === 'rejected' ? t({ id: 'mastodon.admin.relays.enable', defaultMessage: 'Enable' }) : t({ id: 'mastodon.admin.relays.disable', defaultMessage: 'Disable' })}
            </Button>
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.admin.relays.delete_this_relay', defaultMessage: 'Delete this relay?' })}
              description={t({ id: 'eunha.admin.relays.an_enabled_relay_is_unsubscribed_from', defaultMessage: 'An enabled relay is unsubscribed from first.' })}
              confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
              onConfirm={async () => {
                await deleteRelay(token ?? '', relay.id)
                toast.success(t({ id: 'eunha.admin.relays.relay_deleted', defaultMessage: 'Relay deleted.' }))
                load()
              }}
            >
              {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
            </ConfirmButton>
          </div>
        ))}
      </div>
    </AdminLayout>
  )
}
