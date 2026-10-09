// Standard Mastodon OAuth `authorization_code` flow, run client-side via
// masto.js against eunha's existing endpoints. No backend changes required.
import { oauthClient, restClient } from './masto.ts'
import { clearMe, loadMe, type MeAccount } from './me.ts'

// `admin:read` and `admin:write` are what the moderation pages call the admin
// API with. Every account asks for them, as Mastodon's own web client does: the
// scope only says what the token may be used for, and the server still checks
// the account's role before it answers an admin request.
const SCOPES = 'read write follow push admin:read admin:write'
const CLIENT_KEY = 'eunha:client'
const ACTIVE_ACCOUNT_KEY = 'eunha:active-account'
const ACCOUNTS_KEY = 'eunha:accounts'

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
  client_secret: string
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

export function logout() {
  const id = localStorage.getItem(ACTIVE_ACCOUNT_KEY)
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

// Register a first-party OAuth app for this instance once, then reuse it.
async function ensureClient(): Promise<ClientCreds> {
  const existing = storedClient()
  if (existing && existing.scopes === SCOPES) return existing

  const app = await restClient().v1.apps.create({
    clientName: 'Eunha Web',
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

// Kick off login: register (or reuse) the app, then send the browser to the
// server-rendered authorize page. (masto.js doesn't navigate the browser.)
export async function beginLogin(addAccount = false) {
  const { client_id } = await ensureClient()
  const params = new URLSearchParams({
    client_id,
    redirect_uri: redirectUri(),
    response_type: 'code',
    scope: SCOPES,
  })
  if (addAccount) params.set('force_login', 'true')
  window.location.assign(`/oauth/authorize?${params}`)
}

// Exchange the authorization code for a bearer token.
export async function completeLogin(code: string): Promise<void> {
  const creds = storedClient()
  if (!creds) throw new Error('missing client credentials')

  const token = await oauthClient().token.create({
    grantType: 'authorization_code',
    clientId: creds.client_id,
    clientSecret: creds.client_secret,
    redirectUri: redirectUri(),
    code,
    scope: SCOPES,
  })
  // Verify before replacing the active session, so a failed login preserves it.
  const me = await restClient(token.accessToken).v1.accounts.verifyCredentials()
  const account: MeAccount = {
    id: me.id, acct: me.acct, displayName: me.displayName || me.username,
    avatar: me.avatar, defaultVisibility: me.source.privacy ?? 'public',
  }
  rememberAccount(token.accessToken, account)
  clearMe()
  localStorage.setItem(ACTIVE_ACCOUNT_KEY, account.id)
  await loadMe(token.accessToken)
}
