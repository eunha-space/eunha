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

const TITLE = 'Fediverse Auxiliary Service Providers'

/** `admin/fasp/shared/_links`: providers, and the debug callbacks. */
const SUB = [
  { to: '/admin/fasp/providers', label: 'Providers' },
  { to: '/admin/fasp/debug/callbacks', label: 'Debug callbacks' },
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
      toast.success('The provider was asked to call back.')
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title={TITLE} permission="manage_federation" sub={SUB}>
      <p className="text-muted-foreground mb-3 text-sm">
        Providers register themselves with this server. Finish a registration only if you started
        it, and only after comparing its name and key fingerprint with what the provider shows.
      </p>
      <AdminError error={error} />
      {providers === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {providers?.length === 0 && (
        <p className="text-muted-foreground text-sm">No provider has registered yet.</p>
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
              {provider.confirmed ? 'Active' : 'Registration requested'}
            </Badge>
            {provider.confirmed ? (
              <Button
                size="xs"
                variant="outline"
                render={<Link to={`/admin/fasp/providers/${provider.id}/edit`} />}
              >
                Edit
              </Button>
            ) : (
              <Button
                size="xs"
                variant="outline"
                render={<Link to={`/admin/fasp/providers/${provider.id}/registration/new`} />}
              >
                Finish registration
              </Button>
            )}
            {provider.sign_in_url && (
              <Button
                size="xs"
                variant="outline"
                render={<a href={provider.sign_in_url} target="_blank" rel="noreferrer" />}
              >
                Sign in
              </Button>
            )}
            {capabilityEnabled(provider, 'callback') && (
              <Button size="xs" variant="outline" onClick={() => void debugCall(provider)}>
                Callback
              </Button>
            )}
            <ConfirmButton
              size="xs"
              title="Delete this provider?"
              description="Its subscriptions, backfill requests and debug callbacks go with it."
              confirmLabel="Delete"
              onConfirm={async () => {
                await deleteFaspProvider(token ?? '', provider.id)
                toast.success('Provider deleted.')
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
      toast.success('Registration confirmed. Choose the capabilities to use.')
      navigate(`/admin/fasp/providers/${provider.id}/edit`)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <AdminLayout title="Confirm FASP registration" permission="manage_federation" sub={SUB}>
      <AdminError error={error} />
      {provider && (
        <div className="space-y-4">
          <p className="text-sm">
            You received a registration from a FASP. Reject it if you did not initiate this. If you
            initiated this, carefully compare name and key fingerprint before confirming the
            registration.
          </p>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
            <dt className="text-muted-foreground">Name</dt>
            <dd className="min-w-0 break-words">{provider.name}</dd>
            <dt className="text-muted-foreground">Public key fingerprint</dt>
            <dd className="min-w-0 font-mono text-xs break-all">
              {provider.provider_public_key_fingerprint}
            </dd>
          </dl>
          <div className="flex flex-wrap gap-2">
            <ConfirmButton
              title="Reject this registration?"
              description="The provider is deleted, and has to register again."
              confirmLabel="Reject"
              onConfirm={async () => {
                await deleteFaspProvider(token ?? '', provider.id)
                toast.success('Registration rejected.')
                navigate('/admin/fasp/providers')
              }}
            >
              Reject
            </ConfirmButton>
            <Button size="sm" disabled={busy || provider.confirmed} onClick={() => void confirm()}>
              Confirm
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
      toast.success('Capabilities saved.')
      navigate('/admin/fasp/providers')
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title="Edit provider" permission="manage_federation" sub={SUB}>
      <AdminError error={error} />
      {provider && capabilities && (
        <div className="space-y-4">
          <div className="text-sm">
            <div className="font-medium">{provider.name}</div>
            <div className="text-muted-foreground">{provider.base_url}</div>
          </div>
          <h2 className="text-sm font-semibold">Select capabilities</h2>
          {capabilities.length === 0 && (
            <p className="text-muted-foreground text-sm">The provider offers no capabilities.</p>
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
            Save
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
    <AdminLayout title="Debug callbacks" permission="manage_federation" sub={SUB}>
      <AdminError error={error} />
      {callbacks === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {callbacks?.length === 0 && (
        <p className="text-muted-foreground text-sm">No provider has called back yet.</p>
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
                title="Delete this callback?"
                description="Only the record of it is deleted."
                confirmLabel="Delete"
                onConfirm={async () => {
                  await deleteFaspDebugCallback(token ?? '', callback.id)
                  load()
                }}
              >
                Delete
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
