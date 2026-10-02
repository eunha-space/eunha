import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import { can } from '../../admin-api.ts'
import {
  createWebhook,
  deleteWebhook,
  listWebhooks,
  updateWebhook,
  WEBHOOK_EVENTS,
  webhookAction,
  type AdminWebhook,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

function WebhookForm({
  initial,
  onSaved,
  onCancel,
}: {
  initial: AdminWebhook | null
  onSaved: () => void
  onCancel?: () => void
}) {
  const token = getToken()
  const permissions = useRolePermissions() ?? 0
  const [url, setUrl] = useState(initial?.url ?? '')
  const [events, setEvents] = useState<string[]>(initial?.events ?? [])
  const [template, setTemplate] = useState(initial?.template ?? '')
  const [saving, setSaving] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      if (initial) await updateWebhook(token ?? '', initial.id, { url, events, template })
      else {
        await createWebhook(token ?? '', { url, events, template })
        setUrl('')
        setEvents([])
        setTemplate('')
      }
      toast.success(initial ? 'Webhook saved.' : 'Webhook added.')
      onSaved()
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const id = initial?.id ?? 'new'
  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border p-3">
      <div className="space-y-1">
        <Label htmlFor={`webhook-url-${id}`}>Endpoint URL</Label>
        <Input
          id={`webhook-url-${id}`}
          type="url"
          value={url}
          placeholder="https://example.com/webhook"
          onChange={(e) => setUrl(e.target.value)}
        />
      </div>
      <fieldset className="space-y-1">
        <legend className="text-sm font-medium">Enabled events</legend>
        <div className="grid gap-1 sm:grid-cols-2">
          {WEBHOOK_EVENTS.map(({ event, permission }) => (
            <Label key={event} className="text-sm font-normal">
              <Checkbox
                checked={events.includes(event)}
                disabled={!can(permissions, permission)}
                onCheckedChange={(on) =>
                  setEvents((list) => (on ? [...list, event] : list.filter((x) => x !== event)))
                }
              />
              <code>{event}</code>
            </Label>
          ))}
        </div>
      </fieldset>
      <div className="space-y-1">
        <Label htmlFor={`webhook-template-${id}`}>Payload template</Label>
        <Textarea
          id={`webhook-template-${id}`}
          rows={3}
          className="font-mono text-sm"
          value={template}
          placeholder='{"text": "Report {{object.id}}"}'
          onChange={(e) => setTemplate(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          Leave blank for the default JSON payload. Use {'{{object.id}}'}-style paths into the
          event to fill in values.
        </p>
      </div>
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={saving || !url.trim() || events.length === 0}>
          {initial ? 'Save changes' : 'Add endpoint'}
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="outline" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  )
}

/**
 * Webhooks: Mastodon's `Admin::WebhooksController`. Each enabled endpoint is
 * called, signed with its secret, for the events it subscribes to.
 */
export default function Webhooks() {
  const token = getToken()
  const [hooks, setHooks] = useState<AdminWebhook[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | null>(null)
  const [revealed, setRevealed] = useState<Set<string>>(new Set())

  const load = () => {
    if (!token) return
    listWebhooks(token)
      .then((list) => {
        setHooks(list)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  const act = async (id: string, action: 'enable' | 'disable' | 'secret/rotate', done: string) => {
    try {
      await webhookAction(token ?? '', id, action)
      toast.success(done)
      load()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title="Webhooks" permission="manage_webhooks">
      <p className="text-muted-foreground mb-3 text-sm">
        Webhooks send administrative events to an endpoint of yours, signed with the endpoint’s
        secret in <code>X-Hub-Signature</code>.
      </p>
      <div className="mb-4">
        <WebhookForm initial={null} onSaved={load} />
      </div>
      <AdminError error={error} />
      {hooks === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {hooks?.length === 0 && (
        <p className="text-muted-foreground text-sm">No webhook endpoints configured yet.</p>
      )}
      <div className="space-y-2">
        {hooks?.map((hook) =>
          editing === hook.id ? (
            <WebhookForm
              key={hook.id}
              initial={hook}
              onCancel={() => setEditing(null)}
              onSaved={() => {
                setEditing(null)
                load()
              }}
            />
          ) : (
            <div key={hook.id} className="space-y-2 rounded-lg border p-3">
              <div className="flex flex-wrap items-center gap-2">
                <span className="min-w-0 flex-1 truncate text-sm font-medium">{hook.url}</span>
                <Badge variant={hook.enabled ? 'default' : 'outline'}>
                  {hook.enabled ? 'Enabled' : 'Disabled'}
                </Badge>
              </div>
              <div className="flex flex-wrap gap-1">
                {hook.events.map((event) => (
                  <Badge key={event} variant="secondary">
                    {event}
                  </Badge>
                ))}
              </div>
              <div className="text-muted-foreground flex items-center gap-2 text-xs">
                Secret:
                <code>{revealed.has(hook.id) ? hook.secret : '••••••••••••'}</code>
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() =>
                    setRevealed((set) => {
                      const next = new Set(set)
                      if (next.has(hook.id)) next.delete(hook.id)
                      else next.add(hook.id)
                      return next
                    })
                  }
                >
                  {revealed.has(hook.id) ? 'Hide' : 'Show'}
                </Button>
              </div>
              <div className="flex flex-wrap gap-2">
                {hook.can_update && (
                  <Button size="xs" variant="outline" onClick={() => setEditing(hook.id)}>
                    Edit
                  </Button>
                )}
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() =>
                    void act(
                      hook.id,
                      hook.enabled ? 'disable' : 'enable',
                      hook.enabled ? 'Webhook disabled.' : 'Webhook enabled.',
                    )
                  }
                >
                  {hook.enabled ? 'Disable' : 'Enable'}
                </Button>
                <ConfirmButton
                  size="xs"
                  destructive={false}
                  title="Rotate the secret?"
                  description="The endpoint must be given the new secret to check signatures."
                  confirmLabel="Rotate"
                  onConfirm={() => act(hook.id, 'secret/rotate', 'Secret rotated.')}
                >
                  Rotate secret
                </ConfirmButton>
                {hook.can_update && (
                  <ConfirmButton
                    size="xs"
                    title="Delete this webhook?"
                    description="The endpoint is no longer called."
                    confirmLabel="Delete"
                    onConfirm={async () => {
                      await deleteWebhook(token ?? '', hook.id)
                      toast.success('Webhook deleted.')
                      load()
                    }}
                  >
                    Delete
                  </ConfirmButton>
                )}
              </div>
            </div>
          ),
        )}
      </div>
    </AdminLayout>
  )
}
