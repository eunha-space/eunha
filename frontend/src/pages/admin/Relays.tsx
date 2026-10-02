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

const STATES: Record<AdminRelay['state'], string> = {
  idle: 'Disabled',
  pending: 'Waiting for relay’s approval',
  accepted: 'Enabled',
  rejected: 'Rejected by relay',
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
      toast.success('Relay added. It is enabled once the relay approves.')
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
    <AdminLayout title="Relays" permission="manage_federation">
      <p className="text-muted-foreground mb-3 text-sm">
        A federation relay is an intermediary server that exchanges large volumes of public
        posts between servers that subscribe and publish to it. It can help small and medium
        servers discover content from the fediverse. Relays do not work while the server
        requires signed fetches.
      </p>
      <form onSubmit={add} className="mb-4 flex flex-wrap items-end gap-2">
        <div className="min-w-60 flex-1 space-y-1">
          <Label htmlFor="relay-inbox">Relay inbox URL</Label>
          <Input
            id="relay-inbox"
            type="url"
            placeholder="https://relay.example.com/inbox"
            value={inboxUrl}
            onChange={(e) => setInboxUrl(e.target.value)}
          />
        </div>
        <Button type="submit" disabled={saving || !inboxUrl.trim()}>
          Add relay
        </Button>
      </form>
      <AdminError error={error} />
      {relays === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {relays?.length === 0 && (
        <p className="text-muted-foreground text-sm">No relays have been set up yet.</p>
      )}
      <div className="space-y-2">
        {relays?.map((relay) => (
          <div key={relay.id} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
            <span className="min-w-0 flex-1 truncate text-sm font-medium">{relay.inbox_url}</span>
            <Badge variant={relay.enabled ? 'default' : 'outline'}>{STATES[relay.state]}</Badge>
            <Button size="xs" variant="outline" onClick={() => void toggle(relay)}>
              {relay.state === 'idle' || relay.state === 'rejected' ? 'Enable' : 'Disable'}
            </Button>
            <ConfirmButton
              size="xs"
              title="Delete this relay?"
              description="An enabled relay is unsubscribed from first."
              confirmLabel="Delete"
              onConfirm={async () => {
                await deleteRelay(token ?? '', relay.id)
                toast.success('Relay deleted.')
                load()
              }}
            >
              Delete
            </ConfirmButton>
          </div>
        ))}
      </div>
    </AdminLayout>
  )
}
