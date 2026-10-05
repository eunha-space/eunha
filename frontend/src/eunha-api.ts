// eunha-specific APIs with no Mastodon C2S equivalent. Mastodon has no
// invite-tree endpoint nor a REST API for invite CRUD, so these are served by
// eunha's own routes and called with a plain fetch (masto.js only models the
// C2S surface).

// Carries the HTTP status so callers can tell "you typed the wrong password"
// (401) from "something broke", which the message alone doesn't say.
export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

async function eunhaFetch(
  path: string,
  token: string,
  init?: RequestInit,
): Promise<Response> {
  const res = await fetch(`${window.location.origin}${path}`, {
    ...init,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...init?.headers,
    },
  })
  if (!res.ok) {
    throw new ApiError(res.status, `${path} failed: ${res.status}`)
  }
  return res
}

export interface InviteTreeAccount {
  id: string
  username: string
  acct: string
  display_name: string
  avatar: string
  invited_at: string
  root_reason?: 'no_recorded_inviter' | 'inviter_unavailable' | 'lineage_unavailable'
}

export interface InviteNode extends InviteTreeAccount {
  children: InviteNode[]
}

export interface InviteTree {
  roots: InviteNode[]
  total: number
}

export async function getInviteTree(token: string): Promise<InviteTree> {
  const res = await eunhaFetch('/api/eunha/v1/invite_tree', token)
  return res.json() as Promise<InviteTree>
}

export const INVITES_CHANGED = 'eunha:invites-changed'

// ── Invites ────────────────────────────────────────────────────────────────
// Served by eunha's /api/v1/invites (a non-standard extension: Mastodon exposes
// invite CRUD only through its web UI, never the REST API).

export interface Invite {
  id: string
  code: string
  expired: boolean
  valid_for_use: boolean
  url: string
  max_uses: number | null
  uses: number
  expires_at: string | null
  autofollow: boolean
  comment: string | null
  created_at: string
}

export interface CreateInviteParams {
  max_uses?: number
  /** Seconds until expiry; omit for never. */
  expires_in?: number
  autofollow?: boolean
  comment?: string
}

export async function getInvites(token: string): Promise<Invite[]> {
  const res = await eunhaFetch('/api/v1/invites', token)
  return res.json() as Promise<Invite[]>
}

export async function createInvite(
  token: string,
  params: CreateInviteParams,
): Promise<Invite> {
  const res = await eunhaFetch('/api/v1/invites', token, {
    method: 'POST',
    body: JSON.stringify(params),
  })
  const invite = await res.json() as Invite
  window.dispatchEvent(new Event(INVITES_CHANGED))
  return invite
}

export interface GrantInvitesParams {
  /** Whose account to mint them into; omit for every local member. */
  account_id?: string
  count: number
  /** Uses per code; 1 by default. */
  max_uses?: number
  /** Seconds until expiry; omit for never. */
  expires_in?: number
  comment?: string
}

export interface GrantInvitesResult {
  granted: number
  accounts: number
}

/**
 * Mint invites into other members' accounts (admin only).
 *
 * Under `/api/eunha/` because Mastodon has no such action at all: there an
 * invite is made by whoever hands it out. The codes belong to the member they
 * are minted for, so a signup through one lands under them in the invite tree.
 */
export async function grantInvites(
  token: string,
  params: GrantInvitesParams,
): Promise<GrantInvitesResult> {
  const res = await eunhaFetch('/api/eunha/v1/invite_grants', token, {
    method: 'POST',
    body: JSON.stringify(params),
  })
  const result = await res.json() as GrantInvitesResult
  window.dispatchEvent(new Event(INVITES_CHANGED))
  return result
}

export async function deleteInvite(token: string, id: string): Promise<void> {
  await eunhaFetch(`/api/v1/invites/${id}`, token, { method: 'DELETE' })
  window.dispatchEvent(new Event(INVITES_CHANGED))
}

// ── Sign up ─────────────────────────────────────────────────────────────────
// POST /auth is the web sign-up, Mastodon's Auth::RegistrationsController: the
// new user is saved unconfirmed and signed in, and waits on /auth/setup for the
// link mailed to them. (POST /api/v1/accounts is for apps, with a
// client-credentials token.)

export interface SignUpParams {
  username: string
  email: string
  password: string
  locale?: string
  invite_code?: string
  reason?: string
  /** `agreement`: the terms of service and privacy policy were accepted. */
  agreement: boolean
  /** ISO 8601, asked for when the instance sets `registrations.min_age`. */
  date_of_birth?: string
}

export async function signUp(params: SignUpParams): Promise<void> {
  const res = await fetch(`${window.location.origin}/auth`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(params),
  })
  if (!res.ok) {
    let message = `Sign up failed (${res.status})`
    try {
      const body = (await res.json()) as { error?: string }
      if (body.error) message = body.error
    } catch {
      // keep the status-based fallback
    }
    throw new Error(message)
  }
}

// ── Account deletion ───────────────────────────────────────────────────────
// Mastodon deletes accounts through a web form (`/settings/delete`) and has no
// REST equivalent, so `DELETE /api/v1/accounts` is eunha's own. It runs the
// same challenge as that form: the current password, or — for accounts with no
// password — the username.

export interface DeleteAccountChallenge {
  password?: string
  username?: string
}

// Suspends the account immediately and purges it in the background. The
// caller's token stops working as soon as this returns.
export async function deleteAccount(
  token: string,
  challenge: DeleteAccountChallenge,
): Promise<void> {
  await eunhaFetch('/api/v1/accounts', token, {
    method: 'DELETE',
    body: JSON.stringify(challenge),
  })
}

// ── Email subscriptions ────────────────────────────────────────────────────
// Mastodon 4.7. Subscribing is `POST /api/v1/accounts/:id/email_subscriptions`,
// which asks for no token; an account's own switch lives on Mastodon's web
// privacy settings page, so eunha serves it at /api/eunha/v1/email_subscriptions.

/** `ValidationErrorFormatter`'s `details`: per attribute, `ERR_*` codes. */
export type ValidationDetails = Record<string, { error: string; description: string }[]>

export class SubscribeError extends Error {
  constructor(
    public status: number,
    message: string,
    public details: ValidationDetails | null,
  ) {
    super(message)
    this.name = 'SubscribeError'
  }
}

export async function subscribeByEmail(accountId: string, email: string): Promise<void> {
  const res = await fetch(
    `${window.location.origin}/api/v1/accounts/${accountId}/email_subscriptions`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ email }),
    },
  )
  if (res.ok) return
  let message = `Subscribing failed (${res.status})`
  let details: ValidationDetails | null = null
  try {
    const body = (await res.json()) as { error?: string; details?: ValidationDetails }
    if (body.error) message = body.error
    details = body.details ?? null
  } catch {
    // A 404 from a feature that is off has no body.
  }
  throw new SubscribeError(res.status, message, details)
}

export interface OwnEmailSubscriptions {
  /** The feature is enabled and this account's role may use it. */
  available: boolean
  enabled: boolean
  /** Confirmed subscribers. */
  subscribers: number
}

export async function getOwnEmailSubscriptions(token: string): Promise<OwnEmailSubscriptions> {
  const res = await eunhaFetch('/api/eunha/v1/email_subscriptions', token)
  return res.json() as Promise<OwnEmailSubscriptions>
}

export async function setOwnEmailSubscriptions(
  token: string,
  enabled: boolean,
): Promise<OwnEmailSubscriptions> {
  const res = await eunhaFetch('/api/eunha/v1/email_subscriptions', token, {
    method: 'PUT',
    body: JSON.stringify({ enabled }),
  })
  return res.json() as Promise<OwnEmailSubscriptions>
}

// ── Data export and import ─────────────────────────────────────────────────
// Mastodon's "Import and export" settings pages, which have no REST API
// upstream; eunha serves them under /api/eunha/v1/ (docs/operating/import-export.md).

export interface Backup {
  id: string
  processed: boolean
  dump_file_size: number | null
  created_at: string
}

export interface ExportSummary {
  storage: number
  statuses: number
  follows: number
  followers: number
  lists: number
  mutes: number
  blocks: number
  domain_blocks: number
  bookmarks: number
  custom_filters: number
  backups: Backup[]
  can_request_backup: boolean
}

export type ImportType =
  | 'following'
  | 'blocking'
  | 'muting'
  | 'domain_blocking'
  | 'bookmarks'
  | 'lists'
  | 'custom_filters'

export interface BulkImport {
  id: string
  type: ImportType
  state: 'unconfirmed' | 'scheduled' | 'in_progress' | 'finished'
  overwrite: boolean
  original_filename: string
  likely_mismatched: boolean
  missing_status: boolean
  total_items: number
  processed_items: number
  imported_items: number
  failure_count: number
  created_at: string
  finished_at: string | null
}

export async function getExportSummary(token: string): Promise<ExportSummary> {
  const res = await eunhaFetch('/api/eunha/v1/exports', token)
  return res.json() as Promise<ExportSummary>
}

/** Fetch a file the API serves as an attachment, and hand it to the browser to save. */
export async function downloadFile(token: string, path: string): Promise<void> {
  const res = await eunhaFetch(path, token)
  const disposition = res.headers.get('content-disposition') ?? ''
  const name = /filename="([^"]+)"/.exec(disposition)?.[1] ?? 'download'
  const url = URL.createObjectURL(await res.blob())
  const link = document.createElement('a')
  link.href = url
  link.download = name
  link.click()
  URL.revokeObjectURL(url)
}

export async function requestBackup(token: string): Promise<Backup> {
  const res = await eunhaFetch('/api/eunha/v1/backups', token, { method: 'POST' })
  return res.json() as Promise<Backup>
}

export async function getBackupDownloadUrl(token: string, id: string): Promise<string> {
  const res = await eunhaFetch(`/api/eunha/v1/backups/${id}/download`, token)
  const body = (await res.json()) as { url: string }
  return body.url
}

export async function getRecentImports(token: string): Promise<BulkImport[]> {
  const res = await eunhaFetch('/api/eunha/v1/imports', token)
  return res.json() as Promise<BulkImport[]>
}

/** Upload a file to import; a refusal throws with Mastodon's message. */
export async function uploadImport(
  token: string,
  type: ImportType,
  mode: 'merge' | 'overwrite',
  file: File,
): Promise<BulkImport> {
  const form = new FormData()
  form.append('type', type)
  form.append('mode', mode)
  form.append('data', file)
  const res = await fetch(`${window.location.origin}/api/eunha/v1/imports`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}` },
    body: form,
  })
  if (!res.ok) {
    let message = `Upload failed: ${res.status}`
    try {
      message = ((await res.json()) as { error?: string }).error ?? message
    } catch {
      // No JSON body: keep the status.
    }
    throw new ApiError(res.status, message)
  }
  return res.json() as Promise<BulkImport>
}

export async function confirmImport(token: string, id: string): Promise<BulkImport> {
  const res = await eunhaFetch(`/api/eunha/v1/imports/${id}/confirm`, token, {
    method: 'POST',
  })
  return res.json() as Promise<BulkImport>
}

export async function cancelImport(token: string, id: string): Promise<void> {
  await eunhaFetch(`/api/eunha/v1/imports/${id}`, token, { method: 'DELETE' })
}

export interface InviteResolution {
  valid: boolean
  reason?: string
  bypass_approval?: boolean
  autofollow?: boolean
  inviter?: { id: string; acct: string; display_name: string }
}

export async function resolveInvite(code: string, signal?: AbortSignal): Promise<InviteResolution> {
  const res = await fetch(`/api/eunha/v1/invite?invite=${encodeURIComponent(code)}`, { signal })
  if (!res.ok) throw new Error('Could not check this invite. Please try again.')
  return res.json()
}
