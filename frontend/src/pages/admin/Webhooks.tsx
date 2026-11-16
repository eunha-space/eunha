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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

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
      toast.success(initial ? t({ id: 'eunha.admin.webhooks.webhook_saved', defaultMessage: 'Webhook saved.' }) : t({ id: 'eunha.admin.webhooks.webhook_added', defaultMessage: 'Webhook added.' }))
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
        <Label htmlFor={`webhook-url-${id}`}>{t({ id: 'mastodon.simple_form.labels.webhook.url', defaultMessage: 'Endpoint URL' })}</Label>
        <Input
          id={`webhook-url-${id}`}
          type="url"
          value={url}
          placeholder="https://example.com/webhook"
          onChange={(e) => setUrl(e.target.value)}
        />
      </div>
      <fieldset className="space-y-1">
        <legend className="text-sm font-medium">{t({ id: 'mastodon.simple_form.labels.webhook.events', defaultMessage: 'Enabled events' })}</legend>
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
        <Label htmlFor={`webhook-template-${id}`}>{t({ id: 'mastodon.simple_form.labels.webhook.template', defaultMessage: 'Payload template' })}</Label>
        <Textarea
          id={`webhook-template-${id}`}
          rows={3}
          className="font-mono text-sm"
          value={template}
          placeholder={t({ id: 'eunha.admin.webhooks.text', defaultMessage: '\'{\'"text": "Report \'{\'\'{\'object.id\'}\'\'}\'"\'}\'' })}
          onChange={(e) => setTemplate(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          {t(
            { id: 'eunha.admin.webhooks.template_hint', defaultMessage: 'Leave blank for the default JSON payload. Use {example}-style paths into the event to fill in values.' },
            { example: '{{object.id}}' },
          )}
        </p>
      </div>
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={saving || !url.trim() || events.length === 0}>
          {initial ? t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' }) : t({ id: 'mastodon.admin.webhooks.add_new', defaultMessage: 'Add endpoint' })}
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="outline" onClick={onCancel}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
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
    <AdminLayout title={t({ id: 'mastodon.admin.webhooks.title', defaultMessage: 'Webhooks' })} permission="manage_webhooks">
      <p className="text-muted-foreground mb-3 text-sm">
        <FormattedMessage
          id='eunha.admin.webhooks.description'
          defaultMessage='Webhooks send administrative events to an endpoint of yours, signed with the endpoint’s secret in {header}.'
          values={{ header: <code>X-Hub-Signature</code> }}
        />
      </p>
      <div className="mb-4">
        <WebhookForm initial={null} onSaved={load} />
      </div>
      <AdminError error={error} />
      {hooks === null && !error && <RowsSkeleton action />}
      {hooks?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.webhooks.no_webhook_endpoints_configured_yet', defaultMessage: 'No webhook endpoints configured yet.' })}</p>
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
                  {hook.enabled ? t({ id: 'mastodon.admin.relays.enabled', defaultMessage: 'Enabled' }) : t({ id: 'mastodon.admin.webhooks.disabled', defaultMessage: 'Disabled' })}
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
                {t({ id: 'eunha.admin.webhooks.secret', defaultMessage: 'Secret:' })}
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
                  {revealed.has(hook.id) ? t({ id: 'media_gallery.hide', defaultMessage: 'Hide' }) : t({ id: 'content_warning.show_short', defaultMessage: 'Show' })}
                </Button>
              </div>
              <div className="flex flex-wrap gap-2">
                {hook.can_update && (
                  <Button size="xs" variant="outline" onClick={() => setEditing(hook.id)}>
                    {t({ id: 'status.edit', defaultMessage: 'Edit' })}
                  </Button>
                )}
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() =>
                    void act(
                      hook.id,
                      hook.enabled ? 'disable' : 'enable',
                      hook.enabled ? t({ id: 'eunha.admin.webhooks.webhook_disabled', defaultMessage: 'Webhook disabled.' }) : t({ id: 'eunha.admin.webhooks.webhook_enabled', defaultMessage: 'Webhook enabled.' }),
                    )
                  }
                >
                  {hook.enabled ? t({ id: 'mastodon.admin.webhooks.disable', defaultMessage: 'Disable' }) : t({ id: 'mastodon.admin.webhooks.enable', defaultMessage: 'Enable' })}
                </Button>
                <ConfirmButton
                  size="xs"
                  destructive={false}
                  title={t({ id: 'eunha.admin.webhooks.rotate_the_secret', defaultMessage: 'Rotate the secret?' })}
                  description={t({ id: 'eunha.admin.webhooks.the_endpoint_must_be_given_the', defaultMessage: 'The endpoint must be given the new secret to check signatures.' })}
                  confirmLabel={t({ id: 'eunha.admin.webhooks.rotate', defaultMessage: 'Rotate' })}
                  onConfirm={() => act(hook.id, 'secret/rotate', t({ id: 'eunha.admin.webhooks.secret_rotated', defaultMessage: 'Secret rotated.' }))}
                >
                  {t({ id: 'mastodon.admin.webhooks.rotate_secret', defaultMessage: 'Rotate secret' })}
                </ConfirmButton>
                {hook.can_update && (
                  <ConfirmButton
                    size="xs"
                    title={t({ id: 'eunha.admin.webhooks.delete_this_webhook', defaultMessage: 'Delete this webhook?' })}
                    description={t({ id: 'eunha.admin.webhooks.the_endpoint_is_no_longer_called', defaultMessage: 'The endpoint is no longer called.' })}
                    confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                    onConfirm={async () => {
                      await deleteWebhook(token ?? '', hook.id)
                      toast.success(t({ id: 'eunha.admin.webhooks.webhook_deleted', defaultMessage: 'Webhook deleted.' }))
                      load()
                    }}
                  >
                    {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
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
