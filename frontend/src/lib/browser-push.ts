const endpoint = '/api/v1/push/subscription'

export const pushAlertLabels = {
  mention: 'Mentions',
  follow: 'New followers',
  follow_request: 'Follow requests',
  favourite: 'Favourites',
  reblog: 'Boosts',
  poll: 'Poll results',
  quote: 'Quotes',
}
type Alert = keyof typeof pushAlertLabels
type Alerts = Partial<Record<string, boolean>>
type Policy = 'all' | 'followed' | 'follower' | 'none'
export type BrowserPushState = {
  available: boolean
  enabled: boolean
  alerts: Alerts
  policy: Policy
}

function scope(id: string) {
  return `/push/${encodeURIComponent(id)}/`
}

export function browserPushSupported() {
  return window.isSecureContext && 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window
}

async function request(path: string, token: string, method = 'GET', body?: object) {
  const response = await fetch(path, {
    method,
    signal: AbortSignal.timeout(10_000),
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  })
  if (!response.ok && !(response.status === 404 && path === endpoint && method !== 'POST')) {
    throw new Error('Could not update browser notifications')
  }
  return response
}

export async function getBrowserPush(id: string, token: string): Promise<BrowserPushState> {
  const [key, registration, response] = await Promise.all([
    request('/api/eunha/v1/preferences/push_key', token).then((res) => res.json() as Promise<{ public_key: string }>),
    navigator.serviceWorker.getRegistration(scope(id)),
    request(endpoint, token),
  ])
  const subscription = await registration?.pushManager.getSubscription()
  const server = response.ok ? await response.json() as { endpoint: string; alerts: Alerts; policy: Policy } : null
  return {
    available: !!key.public_key,
    enabled: !!subscription && server?.endpoint === subscription.endpoint,
    alerts: server?.alerts ?? {},
    policy: server?.policy ?? 'all',
  }
}

function waitForActivation(registration: ServiceWorkerRegistration) {
  if (registration.active) return Promise.resolve()
  const worker = registration.installing ?? registration.waiting
  if (!worker) return Promise.reject(new Error('Could not start browser notifications'))
  return new Promise<void>((resolve, reject) => {
    const finish = (error?: Error) => {
      clearTimeout(timeout)
      worker.removeEventListener('statechange', onChange)
      if (error) reject(error)
      else resolve()
    }
    const onChange = () => {
      if (worker.state === 'activated') finish()
      if (worker.state === 'redundant') finish(new Error('Could not start browser notifications'))
    }
    const timeout = window.setTimeout(() => finish(new Error('Browser notification setup timed out')), 10_000)
    worker.addEventListener('statechange', onChange)
    onChange()
  })
}

export async function enableBrowserPush(id: string, token: string) {
  if (await Notification.requestPermission() !== 'granted') {
    throw new Error('Browser notification permission was declined')
  }
  const response = await request('/api/eunha/v1/preferences/push_key', token)
  const { public_key } = await response.json() as { public_key: string }
  if (!public_key) throw new Error('This instance has no browser notification key')

  const registration = await navigator.serviceWorker.register('/push-worker.js', { scope: scope(id) })
  await waitForActivation(registration)
  const existing = await registration.pushManager.getSubscription()
  const subscription = existing ?? await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: public_key,
  })
  try {
    await request(endpoint, token, 'POST', {
      subscription: { ...subscription.toJSON(), standard: true },
      data: {
        policy: 'all',
        alerts: Object.fromEntries(Object.keys(pushAlertLabels).map((alert) => [alert, true])),
      },
    })
  } catch (error) {
    if (!existing) await subscription.unsubscribe().catch(() => {})
    throw error
  }
}

export async function setBrowserPushAlert(token: string, state: BrowserPushState, alert: Alert, on: boolean) {
  await request(endpoint, token, 'PUT', {
    data: { policy: state.policy, alerts: { ...state.alerts, [alert]: on } },
  })
}

export async function disableBrowserPush(id: string, token: string) {
  if (!browserPushSupported()) return
  const registration = await navigator.serviceWorker.getRegistration(scope(id))
  const subscription = await registration?.pushManager.getSubscription()
  if (!subscription) return
  await subscription.unsubscribe()
  await request(endpoint, token, 'DELETE')
}
