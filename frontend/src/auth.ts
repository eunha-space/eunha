// Standard Mastodon OAuth `authorization_code` flow, run client-side against
// eunha's own endpoints, with PKCE.
import { restClient } from './masto.ts'
import { disableBrowserPush } from './lib/browser-push.ts'
import { clearMe, loadMe, type MeAccount } from './me.ts'

// `admin:read` and `admin:write` are what the moderation pages call the admin
// API with. Every account asks for them, as Mastodon's own web client does: the
// scope only says what the token may be used for, and the server still checks
// the account's role before it answers an admin request.
const SCOPES = 'read write follow push admin:read admin:write'
const CLIENT_KEY = 'eunha:client'
const ACTIVE_ACCOUNT_KEY = 'eunha:active-account'
const ACCOUNTS_KEY = 'eunha:accounts'
const RETURN_PATH_KEY = 'eunha:return-path'
const VERIFIER_KEY = 'eunha:code-verifier'

export interface SavedAccount {
  token: string
  account: MeAccount
}

export function getSavedAccounts(): SavedAccount[] {
  try {
    const entries: unknown = JSON.parse(localStorage.getItem(ACCOUNTS_KEY) ?? '[]')
    if (!Array.isArray(entries)) return []
    return entries.filter((entry): entry is SavedAccount =>
      typeof entry?.token === 'string' && typeof entry?.account?.id === 'string' &&
      typeof entry?.account?.acct === 'string',
    )
  } catch {
    return []
  }
}

export function rememberAccount(token: string, account: MeAccount) {
  const others = getSavedAccounts().filter((entry) => entry.account.id !== account.id)
  localStorage.setItem(ACCOUNTS_KEY, JSON.stringify([...others, { token, account }]))
}

export function switchAccount(id: string) {
  const saved = getSavedAccounts().find((entry) => entry.account.id === id)
  if (!saved) return
  clearMe()
  localStorage.setItem(ACTIVE_ACCOUNT_KEY, id)
  window.location.assign('/')
}

// All tabs share the active token. Reset mounted user state when another tab
// switches or signs out, including its streaming connections and drafts.
window.addEventListener('storage', (event) => {
  if (event.key === ACTIVE_ACCOUNT_KEY || event.key === ACCOUNTS_KEY || event.key === null) {
    clearMe()
    window.location.assign('/')
  }
})

interface ClientCreds {
  client_id: string
  // None for the instance's own app, which is public: PKCE proves the code.
  client_secret?: string
  // What the app was registered for. An authorization may not ask for more
  // than that, so an app registered before the scopes grew is registered again.
  scopes?: string
}

const redirectUri = () => `${window.location.origin}/auth/callback`

/** The id of the account this tab acts as, before `loadMe` has confirmed it. */
export function getActiveAccountId(): string | null {
  return localStorage.getItem(ACTIVE_ACCOUNT_KEY)
}

export function getToken(): string | null {
  const id = localStorage.getItem(ACTIVE_ACCOUNT_KEY)
  return getSavedAccounts().find((entry) => entry.account.id === id)?.token ?? null
}

export async function logout() {
  const id = localStorage.getItem(ACTIVE_ACCOUNT_KEY)
  const token = getToken()
  if (id && token) await disableBrowserPush(id, token).catch(() => {})

  localStorage.setItem(ACCOUNTS_KEY, JSON.stringify(
    getSavedAccounts().filter((entry) => entry.account.id !== id),
  ))
  const remaining = getSavedAccounts()[0]
  if (remaining) localStorage.setItem(ACTIVE_ACCOUNT_KEY, remaining.account.id)
  else localStorage.removeItem(ACTIVE_ACCOUNT_KEY)
  clearMe()
}

function storedClient(): ClientCreds | null {
  const raw = localStorage.getItem(CLIENT_KEY)
  return raw ? (JSON.parse(raw) as ClientCreds) : null
}

// The instance's own app (`GET /api/eunha/v1/web_app`), which the
// authorization page answers at once, when it is for this origin. Elsewhere,
// as on a development server, an app registered for this browser once and
// then reused, which the page asks the person to authorize.
async function ensureClient(): Promise<ClientCreds> {
  const own = await fetch('/api/eunha/v1/web_app', { headers: { Accept: 'application/json' } })
    .then((res) => (res.ok ? res.json() : null))
    .catch(() => null) as { client_id?: string, redirect_uri?: string, scopes?: string } | null
  if (own?.client_id && own.redirect_uri === redirectUri() && own.scopes === SCOPES) {
    const creds: ClientCreds = { client_id: own.client_id, scopes: SCOPES }
    localStorage.setItem(CLIENT_KEY, JSON.stringify(creds))
    return creds
  }

  const existing = storedClient()
  if (existing?.client_secret && existing.scopes === SCOPES) return existing

  const app = await restClient().v1.apps.create({
    clientName: 'eunha',
    redirectUris: redirectUri(),
    scopes: SCOPES,
    website: window.location.origin,
  })
  if (!app.clientId || !app.clientSecret) {
    throw new Error('app registration returned no credentials')
  }
  const creds: ClientCreds = {
    client_id: app.clientId,
    client_secret: app.clientSecret,
    scopes: SCOPES,
  }
  localStorage.setItem(CLIENT_KEY, JSON.stringify(creds))
  return creds
}

function base64url(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

// A PKCE verifier kept for the callback, and its `S256` challenge.
async function codeChallenge(): Promise<string> {
  const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)))
  sessionStorage.setItem(VERIFIER_KEY, verifier)
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier))
  return base64url(new Uint8Array(digest))
}

// Kick off login: register (or reuse) the app, then send the browser to the
// server-rendered authorize page. (masto.js doesn't navigate the browser.)
export async function beginLogin(addAccount = false, returnPath?: string) {
  const { client_id } = await ensureClient()
  if (returnPath) sessionStorage.setItem(RETURN_PATH_KEY, returnPath)
  const params = new URLSearchParams({
    client_id,
    redirect_uri: redirectUri(),
    response_type: 'code',
    scope: SCOPES,
    code_challenge: await codeChallenge(),
    code_challenge_method: 'S256',
  })
  const authorize = `/oauth/authorize?${params}`
  if (!addAccount) {
    window.location.assign(authorize)
    return
  }
  // Adding an account: the browser is most likely still signed in to the
  // account pages as the current account, and the authorization page would
  // only offer to authorize that account again. Sign the browser out of those
  // pages first; the logout's `continue` brings it on to the authorization
  // page, which then asks for the other account's email and password. This
  // ends only the cookie session, not the tokens saved in the switcher.
  params.set('force_login', 'true')
  const form = document.createElement('form')
  form.method = 'POST'
  form.action = '/account/logout'
  const input = document.createElement('input')
  input.type = 'hidden'
  input.name = 'continue'
  input.value = `/oauth/authorize?${params}`
  form.append(input)
  document.body.append(form)
  form.submit()
}

// Where to go once signed in: the page that asked for it, which is always one
// of this site's own paths, or home.
export function takeReturnPath(): string {
  const path = sessionStorage.getItem(RETURN_PATH_KEY)
  sessionStorage.removeItem(RETURN_PATH_KEY)
  return path && path.startsWith('/') && !path.startsWith('//') ? path : '/'
}

// Exchange the authorization code for a bearer token.
export async function completeLogin(code: string): Promise<void> {
  const creds = storedClient()
  if (!creds) throw new Error('missing client credentials')

  const verifier = sessionStorage.getItem(VERIFIER_KEY)
  sessionStorage.removeItem(VERIFIER_KEY)
  const body = new URLSearchParams({
    grant_type: 'authorization_code',
    client_id: creds.client_id,
    redirect_uri: redirectUri(),
    code,
    scope: SCOPES,
  })
  if (creds.client_secret) body.set('client_secret', creds.client_secret)
  if (verifier) body.set('code_verifier', verifier)
  const res = await fetch('/oauth/token', { method: 'POST', body })
  if (!res.ok) throw new Error(`token exchange failed: ${res.status}`)
  const token = { accessToken: ((await res.json()) as { access_token: string }).access_token }
  // Verify before replacing the active session, so a failed login preserves it.
  const me = await restClient(token.accessToken).v1.accounts.verifyCredentials()
  const account: MeAccount = {
    id: me.id, acct: me.acct, displayName: me.displayName || me.username,
    avatar: me.avatar, avatarStatic: me.avatarStatic, emojis: me.emojis,
    defaultVisibility: me.source.privacy ?? 'public',
  }
  rememberAccount(token.accessToken, account)
  clearMe()
  localStorage.setItem(ACTIVE_ACCOUNT_KEY, account.id)
  await loadMe(token.accessToken)
}
