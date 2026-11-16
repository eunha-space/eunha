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
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const STATES: Record<AdminRelay['state'], string> = {
  get idle() { return t({ id: 'mastodon.admin.relays.disabled', defaultMessage: 'Disabled' }) },
  get pending() { return t({ id: 'eunha.admin.relays.waiting_for_relay_s_approval', defaultMessage: 'Waiting for relay’s approval' }) },
  get accepted() { return t({ id: 'mastodon.admin.relays.enabled', defaultMessage: 'Enabled' }) },
  get rejected() { return t({ id: 'eunha.admin.relays.rejected_by_relay', defaultMessage: 'Rejected by relay' }) },
}

/**
 * Relays: Mastodon's `Admin::RelaysController`. A relay passes public posts
 * between the servers that subscribe to it.
 */
export default function Relays() {
  const token = getToken()
  const [relays, setRelays] = useState<AdminRelay[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [inboxUrl, setInboxUrl] = useState('')
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
      await createRelay(token ?? '', inboxUrl)
      setInboxUrl('')
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
      <form onSubmit={add} className="mb-4 flex flex-wrap items-end gap-2">
        <div className="min-w-60 flex-1 space-y-1">
          <Label htmlFor="relay-inbox">{t({ id: 'eunha.admin.relays.relay_inbox_url', defaultMessage: 'Relay inbox URL' })}</Label>
          <Input
            id="relay-inbox"
            type="url"
            placeholder="https://relay.example.com/inbox"
            value={inboxUrl}
            onChange={(e) => setInboxUrl(e.target.value)}
          />
        </div>
        <Button type="submit" disabled={saving || !inboxUrl.trim()}>
          {t({ id: 'eunha.admin.relays.add_relay', defaultMessage: 'Add relay' })}
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
            <span className="min-w-0 flex-1 truncate text-sm font-medium">{relay.inbox_url}</span>
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
