import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  confirmFaspRegistration,
  deleteFaspDebugCallback,
  deleteFaspProvider,
  getFaspProvider,
  listFaspDebugCallbacks,
  listFaspProviders,
  performFaspDebugCall,
  updateFaspCapabilities,
  type AdminFaspDebugCallback,
  type AdminFaspProvider,
  type FaspCapability,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const TITLE = { id: 'mastodon.admin.fasp.providers.title', defaultMessage: 'Fediverse Auxiliary Service Providers' }

/** `admin/fasp/shared/_links`: providers, and the debug callbacks. */
const SUB = [
  { to: '/admin/fasp/providers', get label() { return t({ id: 'mastodon.admin.fasp.providers.providers', defaultMessage: 'Providers' }) } },
  { to: '/admin/fasp/debug/callbacks', get label() { return t({ id: 'eunha.admin.fasp.debug_callbacks', defaultMessage: 'Debug callbacks' }) } },
]

function capabilityEnabled(provider: AdminFaspProvider, id: string) {
  return provider.confirmed && provider.capabilities.some((c) => c.id === id && c.enabled)
}

/** `Admin::Fasp::ProvidersController#index`. */
export function FaspProviders() {
  const token = getToken()
  const [providers, setProviders] = useState<AdminFaspProvider[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = () => {
    if (!token) return
    listFaspProviders(token)
      .then((p) => {
        setProviders(p)
        setError(null)
      })
      .catch((e) => setError(errorMessage(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  const debugCall = async (provider: AdminFaspProvider) => {
    try {
      await performFaspDebugCall(token ?? '', provider.id)
      toast.success(t({ id: 'eunha.admin.fasp.the_provider_was_asked_to_call', defaultMessage: 'The provider was asked to call back.' }))
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title={t(TITLE)} permission="manage_federation" sub={SUB}>
      <p className="text-muted-foreground mb-3 text-sm">
        {t({ id: 'eunha.admin.fasp.providers_register_themselves_with_this_server', defaultMessage: 'Providers register themselves with this server. Finish a registration only if you started it, and only after comparing its name and key fingerprint with what the provider shows.' })}
      </p>
      <AdminError error={error} />
      {providers === null && !error && <RowsSkeleton action />}
      {providers?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.fasp.no_provider_has_registered_yet', defaultMessage: 'No provider has registered yet.' })}</p>
      )}
      <div className="space-y-2">
        {providers?.map((provider) => (
          <div
            key={provider.id}
            className="flex flex-wrap items-center gap-2 rounded-lg border p-3"
          >
            <div className="min-w-0 flex-1">
              <div className="truncate text-sm font-medium">{provider.name}</div>
              <div className="text-muted-foreground truncate text-xs">{provider.base_url}</div>
            </div>
            <Badge variant={provider.confirmed ? 'default' : 'outline'}>
              {provider.confirmed ? t({ id: 'mastodon.admin.fasp.providers.active', defaultMessage: 'Active' }) : t({ id: 'mastodon.admin.fasp.providers.registration_requested', defaultMessage: 'Registration requested' })}
            </Badge>
            {provider.confirmed ? (
              <Button
                size="xs"
                variant="outline"
                render={<Link to={`/admin/fasp/providers/${provider.id}/edit`} />}
              >
                {t({ id: 'status.edit', defaultMessage: 'Edit' })}
              </Button>
            ) : (
              <Button
                size="xs"
                variant="outline"
                render={<Link to={`/admin/fasp/providers/${provider.id}/registration/new`} />}
              >
                {t({ id: 'mastodon.admin.fasp.providers.finish_registration', defaultMessage: 'Finish registration' })}
              </Button>
            )}
            {provider.sign_in_url && (
              <Button
                size="xs"
                variant="outline"
                render={<a href={provider.sign_in_url} target="_blank" rel="noreferrer" />}
              >
                {t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}
              </Button>
            )}
            {capabilityEnabled(provider, 'callback') && (
              <Button size="xs" variant="outline" onClick={() => void debugCall(provider)}>
                {t({ id: 'mastodon.admin.fasp.providers.callback', defaultMessage: 'Callback' })}
              </Button>
            )}
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.admin.fasp.delete_this_provider', defaultMessage: 'Delete this provider?' })}
              description={t({ id: 'eunha.admin.fasp.its_subscriptions_backfill_requests_and_debug', defaultMessage: 'Its subscriptions, backfill requests and debug callbacks go with it.' })}
              confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
              onConfirm={async () => {
                await deleteFaspProvider(token ?? '', provider.id)
                toast.success(t({ id: 'eunha.admin.fasp.provider_deleted', defaultMessage: 'Provider deleted.' }))
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

/** Loads the provider the route names. */
function useProvider(): [AdminFaspProvider | null, string | null] {
  const token = getToken()
  const { id = '' } = useParams()
  const [provider, setProvider] = useState<AdminFaspProvider | null>(null)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    if (!token) return
    getFaspProvider(token, id)
      .then(setProvider)
      .catch((e) => setError(errorMessage(e)))
  }, [token, id])
  return [provider, error]
}

/** `Admin::Fasp::RegistrationsController#new`: confirm or reject. */
export function FaspRegistration() {
  const token = getToken()
  const navigate = useNavigate()
  const [provider, error] = useProvider()
  const [busy, setBusy] = useState(false)

  const confirm = async () => {
    if (!provider) return
    setBusy(true)
    try {
      await confirmFaspRegistration(token ?? '', provider.id)
      toast.success(t({ id: 'eunha.admin.fasp.registration_confirmed_choose_the_capabilities_to', defaultMessage: 'Registration confirmed. Choose the capabilities to use.' }))
      navigate(`/admin/fasp/providers/${provider.id}/edit`)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <AdminLayout title={t({ id: 'eunha.admin.fasp.confirm_fasp_registration', defaultMessage: 'Confirm FASP registration' })} permission="manage_federation" sub={SUB}>
      <AdminError error={error} />
      {provider && (
        <div className="space-y-4">
          <p className="text-sm">
            {t({ id: 'mastodon.admin.fasp.providers.registrations.description', defaultMessage: 'You received a registration from a FASP. Reject it if you did not initiate this. If you initiated this, carefully compare name and key fingerprint before confirming the registration.' })}
          </p>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
            <dt className="text-muted-foreground">{t({ id: 'mastodon.admin.fasp.providers.name', defaultMessage: 'Name' })}</dt>
            <dd className="min-w-0 break-words">{provider.name}</dd>
            <dt className="text-muted-foreground">{t({ id: 'mastodon.admin.fasp.providers.public_key_fingerprint', defaultMessage: 'Public key fingerprint' })}</dt>
            <dd className="min-w-0 font-mono text-xs break-all">
              {provider.provider_public_key_fingerprint}
            </dd>
          </dl>
          <div className="flex flex-wrap gap-2">
            <ConfirmButton
              title={t({ id: 'eunha.admin.fasp.reject_this_registration', defaultMessage: 'Reject this registration?' })}
              description={t({ id: 'eunha.admin.fasp.the_provider_is_deleted_and_has', defaultMessage: 'The provider is deleted, and has to register again.' })}
              confirmLabel={t({ id: 'follow_request.reject', defaultMessage: 'Reject' })}
              onConfirm={async () => {
                await deleteFaspProvider(token ?? '', provider.id)
                toast.success(t({ id: 'eunha.admin.fasp.registration_rejected', defaultMessage: 'Registration rejected.' }))
                navigate('/admin/fasp/providers')
              }}
            >
              {t({ id: 'follow_request.reject', defaultMessage: 'Reject' })}
            </ConfirmButton>
            <Button size="sm" disabled={busy || provider.confirmed} onClick={() => void confirm()}>
              {t({ id: 'mastodon.admin.fasp.providers.registrations.confirm', defaultMessage: 'Confirm' })}
            </Button>
          </div>
        </div>
      )}
    </AdminLayout>
  )
}

/** `Admin::Fasp::ProvidersController#edit`: the capabilities to use. */
export function FaspProviderEdit() {
  const token = getToken()
  const navigate = useNavigate()
  const [provider, error] = useProvider()
  const [capabilities, setCapabilities] = useState<FaspCapability[] | null>(null)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (provider) setCapabilities(provider.capabilities)
  }, [provider])

  const save = async () => {
    if (!provider || !capabilities) return
    setSaving(true)
    try {
      await updateFaspCapabilities(token ?? '', provider.id, capabilities)
      toast.success(t({ id: 'eunha.admin.fasp.capabilities_saved', defaultMessage: 'Capabilities saved.' }))
      navigate('/admin/fasp/providers')
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title={t({ id: 'eunha.admin.fasp.edit_provider', defaultMessage: 'Edit provider' })} permission="manage_federation" sub={SUB}>
      <AdminError error={error} />
      {provider && capabilities && (
        <div className="space-y-4">
          <div className="text-sm">
            <div className="font-medium">{provider.name}</div>
            <div className="text-muted-foreground">{provider.base_url}</div>
          </div>
          <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.fasp.select_capabilities', defaultMessage: 'Select capabilities' })}</h2>
          {capabilities.length === 0 && (
            <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.fasp.the_provider_offers_no_capabilities', defaultMessage: 'The provider offers no capabilities.' })}</p>
          )}
          <div className="space-y-2">
            {capabilities.map((capability, index) => (
              <Label key={`${capability.id}-${capability.version}`} className="gap-2">
                <Switch
                  checked={capability.enabled}
                  disabled={saving}
                  onCheckedChange={(on) =>
                    setCapabilities(
                      capabilities.map((c, i) => (i === index ? { ...c, enabled: on } : c)),
                    )
                  }
                />
                <span className="font-mono text-xs">{capability.id}</span>
                <span className="text-muted-foreground text-xs">v{capability.version}</span>
              </Label>
            ))}
          </div>
          <Button size="sm" disabled={saving} onClick={() => void save()}>
            {t({ id: 'lists.save', defaultMessage: 'Save' })}
          </Button>
        </div>
      )}
    </AdminLayout>
  )
}

/** `Admin::Fasp::Debug::CallbacksController#index`. */
export function FaspDebugCallbacks() {
  const token = getToken()
  const [callbacks, setCallbacks] = useState<AdminFaspDebugCallback[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = () => {
    if (!token) return
    listFaspDebugCallbacks(token)
      .then((c) => {
        setCallbacks(c)
        setError(null)
      })
      .catch((e) => setError(errorMessage(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  return (
    <AdminLayout title={t({ id: 'eunha.admin.fasp.debug_callbacks', defaultMessage: 'Debug callbacks' })} permission="manage_federation" sub={SUB}>
      <AdminError error={error} />
      {callbacks === null && !error && <RowsSkeleton action />}
      {callbacks?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.fasp.no_provider_has_called_back_yet', defaultMessage: 'No provider has called back yet.' })}</p>
      )}
      <div className="space-y-2">
        {callbacks?.map((callback) => (
          <div key={callback.id} className="space-y-1 rounded-lg border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <span className="min-w-0 flex-1 truncate text-sm font-medium">
                {callback.provider.name}
                <span className="text-muted-foreground ml-2 text-xs font-normal">
                  {callback.provider.base_url}
                </span>
              </span>
              <span className="text-muted-foreground text-xs">{callback.ip}</span>
              <span className="text-muted-foreground text-xs">
                {formatDate(callback.created_at)}
              </span>
              <ConfirmButton
                size="xs"
                title={t({ id: 'eunha.admin.fasp.delete_this_callback', defaultMessage: 'Delete this callback?' })}
                description={t({ id: 'eunha.admin.fasp.only_the_record_of_it_is', defaultMessage: 'Only the record of it is deleted.' })}
                confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                onConfirm={async () => {
                  await deleteFaspDebugCallback(token ?? '', callback.id)
                  load()
                }}
              >
                {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
              </ConfirmButton>
            </div>
            <code className="bg-muted block overflow-x-auto rounded p-2 text-xs break-all whitespace-pre-wrap">
              {callback.request_body}
            </code>
          </div>
        ))}
      </div>
    </AdminLayout>
  )
}
