// Mastodon's admin REST API (`/api/v1/admin/*`, `/api/v2/admin/accounts`),
// which the moderation pages are built on.
//
// Called with a plain fetch rather than masto.js: masto models only part of
// this surface — no v2 account list, no report update, no account deletion, no
// tags, no publishers — and what it does model it hands back in camelCase,
// which would leave these pages reading two spellings of the same entities.
// The shapes here are Mastodon's serializers (`REST::Admin::*`), snake_case as
// they come off the wire.
import { ApiError } from './eunha-api.ts'

// ── Permissions ────────────────────────────────────────────────────────────

/** Mastodon `UserRole::FLAGS`. */
export const PERMISSION = {
  administrator: 1 << 0,
  view_devops: 1 << 1,
  view_audit_log: 1 << 2,
  view_dashboard: 1 << 3,
  manage_reports: 1 << 4,
  manage_federation: 1 << 5,
  manage_settings: 1 << 6,
  manage_blocks: 1 << 7,
  manage_taxonomies: 1 << 8,
  manage_appeals: 1 << 9,
  manage_users: 1 << 10,
  manage_invites: 1 << 11,
  manage_rules: 1 << 12,
  manage_announcements: 1 << 13,
  manage_custom_emojis: 1 << 14,
  manage_webhooks: 1 << 15,
  invite_users: 1 << 16,
  manage_roles: 1 << 17,
  manage_user_access: 1 << 18,
  delete_user_data: 1 << 19,
  view_feeds: 1 << 20,
  invite_bypass_approval: 1 << 21,
  manage_email_subscriptions: 1 << 22,
} as const

export type Permission = keyof typeof PERMISSION

/** `UserRole#can?`: `administrator` grants every other flag. */
export function can(permissions: number, flag: Permission): boolean {
  return (
    (permissions & PERMISSION.administrator) !== 0 ||
    (permissions & PERMISSION[flag]) !== 0
  )
}

// ── Transport ──────────────────────────────────────────────────────────────

/**
 * A failed admin call, with the body the server sent.
 *
 * Mastodon puts the useful half of a refusal in the body — `{"error": …}` —
 * and one response carries more than a message: creating a domain block that
 * an existing one already covers answers 422 with that block attached, which
 * the form offers to edit instead.
 */
export class AdminApiError extends ApiError {
  constructor(
    status: number,
    message: string,
    public body: unknown,
  ) {
    super(status, message)
    this.name = 'AdminApiError'
  }

  /**
   * Whether the token is missing the admin scopes rather than the account
   * missing the role. Tokens issued before the web client asked for
   * `admin:read` and `admin:write` lack them, and signing in again fixes it.
   */
  get outsideScopes(): boolean {
    return this.status === 403 && /outside the authorized scopes/i.test(this.message)
  }
}

function query(params?: Record<string, unknown>): string {
  if (!params) return ''
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === '') continue
    if (Array.isArray(value)) for (const v of value) search.append(`${key}[]`, String(v))
    else search.append(key, String(value))
  }
  const s = search.toString()
  return s ? `?${s}` : ''
}

async function request(
  token: string,
  method: string,
  path: string,
  body?: unknown,
): Promise<Response> {
  const isForm = body instanceof FormData
  const res = await fetch(`${window.location.origin}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body !== undefined && !isForm ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body === undefined ? undefined : isForm ? body : JSON.stringify(body),
  })
  if (!res.ok) {
    let parsed: unknown = null
    try {
      parsed = await res.json()
    } catch {
      // Not every failure has a JSON body.
    }
    const message =
      typeof parsed === 'object' &&
      parsed !== null &&
      'error' in parsed &&
      typeof (parsed as { error: unknown }).error === 'string'
        ? (parsed as { error: string }).error
        : `${method} ${path} failed: ${res.status}`
    throw new AdminApiError(res.status, message, parsed)
  }
  return res
}

async function json<T>(
  token: string,
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  const res = await request(token, method, path, body)
  return res.json() as Promise<T>
}

/** For the calls Mastodon answers with an empty object (`render_empty`). */
async function empty(token: string, method: string, path: string, body?: unknown) {
  await request(token, method, path, body)
}

// The `rel="next"` URL of a `Link` header, as a path on this origin. The
// server writes absolute URLs under its own configured domain; in development
// the SPA is served from a proxy on another one, so only the path and query
// are kept.
function nextLink(res: Response): string | null {
  const header = res.headers.get('Link')
  if (!header) return null
  for (const part of header.split(',')) {
    const match = /<([^>]+)>\s*;\s*rel="?next"?/.exec(part)
    if (match) {
      const url = new URL(match[1], window.location.origin)
      return `${url.pathname}${url.search}`
    }
  }
  return null
}

/**
 * Walk a `Link`-paginated list, one page per step.
 *
 * The shape `useInfinitePaginator` consumes — the same one masto's paginator
 * has — so these lists reuse the hook the blocked and muted pages use.
 */
export function paginate<T>(
  token: string,
  path: string,
  params?: Record<string, unknown>,
): AsyncIterable<T[]> {
  return {
    async *[Symbol.asyncIterator]() {
      let next: string | null = `${path}${query(params)}`
      while (next) {
        const res = await request(token, 'GET', next)
        const page = (await res.json()) as T[]
        yield page
        next = page.length > 0 ? nextLink(res) : null
      }
    },
  }
}

// ── Entities ───────────────────────────────────────────────────────────────

/** `REST::AccountSerializer`, the fields these pages read. */
export interface Account {
  id: string
  username: string
  acct: string
  display_name: string
  avatar: string
  avatar_static?: string
  url: string
  note?: string
  locked?: boolean
  bot?: boolean
  created_at: string
  followers_count?: number
  following_count?: number
  statuses_count?: number
  last_status_at?: string | null
}

export interface Role {
  id: string
  name: string
  color?: string
  permissions?: string
  highlighted?: boolean
}

/** `REST::Admin::AccountSerializer`. */
export interface AdminAccount {
  id: string
  username: string
  domain: string | null
  created_at: string
  email: string | null
  ip: string | null
  ips: { ip: string; used_at: string }[]
  role: Role | null
  confirmed: boolean
  suspended: boolean
  silenced: boolean
  sensitized: boolean
  disabled: boolean
  approved: boolean
  locale: string | null
  invite_request: string | null
  created_by_application_id?: string
  invited_by_account_id?: string
  account: Account
}

/**
 * The public account inside an admin account, whichever one a response sent.
 *
 * A report's four accounts are admin accounts in Mastodon. A server that sends
 * the plain account there instead still renders, rather than a page of blanks.
 */
export function publicAccount(a: AdminAccount | Account): Account {
  return 'account' in a && a.account ? a.account : (a as Account)
}

export interface Rule {
  id: string
  text: string
  hint?: string
}

/** `REST::StatusSerializer`, the fields a report shows. */
export interface Status {
  id: string
  created_at: string
  url: string | null
  uri: string
  content: string
  spoiler_text: string
  sensitive: boolean
  visibility: string
  account: Account
  media_attachments: {
    id: string
    type: string
    url: string | null
    preview_url: string | null
    description: string | null
  }[]
  reblog?: Status | null
}

export type ReportCategory = 'spam' | 'legal' | 'violation' | 'other'

/** `REST::Admin::ReportSerializer`. */
export interface AdminReport {
  id: string
  action_taken: boolean
  action_taken_at: string | null
  category: ReportCategory
  comment: string
  forwarded: boolean
  created_at: string
  updated_at: string
  account: AdminAccount
  target_account: AdminAccount
  assigned_account: AdminAccount | null
  action_taken_by_account: AdminAccount | null
  statuses: Status[]
  rules: Rule[]
}

export type DomainBlockSeverity = 'noop' | 'silence' | 'suspend'

export interface DomainBlock {
  id: string
  domain: string
  digest: string
  created_at: string
  severity: DomainBlockSeverity
  reject_media: boolean
  reject_reports: boolean
  private_comment: string | null
  public_comment: string | null
  obfuscate: boolean
}

export interface DomainAllow {
  id: string
  domain: string
  created_at: string
}

export type IpBlockSeverity = 'sign_up_requires_approval' | 'sign_up_block' | 'no_access'

export interface IpBlock {
  id: string
  ip: string
  severity: IpBlockSeverity
  comment: string
  created_at: string
  expires_at: string | null
}

export interface History {
  day: string
  uses: string
  accounts: string
}

export interface EmailDomainBlock {
  id: string
  domain: string
  created_at: string
  history: History[]
  allow_with_approval: boolean
}

export interface CanonicalEmailBlock {
  id: string
  canonical_email_hash: string
}

/** `REST::Admin::TagSerializer`. */
export interface AdminTag {
  id: string
  name: string
  url: string
  history: History[]
  trendable: boolean
  usable: boolean
  requires_review: boolean
  listable: boolean | null
}

/** `REST::Admin::Trends::LinkSerializer`. */
export interface TrendLink {
  id: string
  url: string
  title: string
  description: string
  provider_name: string
  image: string | null
  history: History[]
  requires_review: boolean
}

/** `REST::Admin::Trends::Links::PreviewCardProviderSerializer`. */
export interface Publisher {
  id: string
  domain: string
  trendable: boolean
  reviewed_at: string | null
  requested_review_at: string | null
  requires_review: boolean
}

export type TrendStatus = Status & { requires_review: boolean }

export interface Measure {
  key: string
  unit: string | null
  total: string
  human_value?: string
  previous_total?: string
  data: { date: string; value: string }[]
}

export interface Dimension {
  key: string
  data: {
    key: string
    human_key: string
    value: string
    unit?: string
    human_value?: string
  }[]
}

export interface Cohort {
  period: string
  frequency: 'day' | 'month'
  data: { date: string; rate: number; value: string }[]
}

/**
 * A custom emoji as eunha's `/api/v1/admin/custom_emojis` serves it.
 *
 * Not a Mastodon API: Mastodon manages custom emoji only through its web UI,
 * so this is eunha's own extension, under the path the admin API would use.
 */
export interface AdminCustomEmoji {
  id: string
  shortcode: string
  url: string
  static_url: string
  visible_in_picker: boolean
  disabled: boolean
  category: string | null
}

// ── Accounts ───────────────────────────────────────────────────────────────

export interface AccountFilters {
  origin?: 'local' | 'remote'
  status?: 'active' | 'pending' | 'disabled' | 'silenced' | 'suspended' | 'sensitized'
  permissions?: 'staff'
  username?: string
  display_name?: string
  by_domain?: string
  email?: string
  ip?: string
  limit?: number
}

export function listAccounts(token: string, filters: AccountFilters) {
  return paginate<AdminAccount>(token, '/api/v2/admin/accounts', { ...filters })
}

export function getAccount(token: string, id: string) {
  return json<AdminAccount>(token, 'GET', `/api/v1/admin/accounts/${id}`)
}

export type AccountUndo = 'enable' | 'unsilence' | 'unsuspend' | 'unsensitive' | 'approve'

export function undoAccount(token: string, id: string, action: AccountUndo) {
  return json<AdminAccount>(token, 'POST', `/api/v1/admin/accounts/${id}/${action}`)
}

/** Deletes the account outright, as declining a sign-up does. */
export function rejectAccount(token: string, id: string) {
  return empty(token, 'POST', `/api/v1/admin/accounts/${id}/reject`)
}

/** Queues the account's data for deletion; it must be suspended first. */
export function deleteAccount(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/accounts/${id}`)
}

export type AccountActionType = 'none' | 'disable' | 'sensitive' | 'silence' | 'suspend'

export interface AccountActionParams {
  type: AccountActionType
  report_id?: string
  warning_preset_id?: string
  text?: string
  send_email_notification?: boolean
}

export function accountAction(token: string, id: string, params: AccountActionParams) {
  return empty(token, 'POST', `/api/v1/admin/accounts/${id}/action`, params)
}

// ── Reports ────────────────────────────────────────────────────────────────

export interface ReportFilters {
  resolved?: boolean
  account_id?: string
  target_account_id?: string
  limit?: number
}

export function listReports(token: string, filters: ReportFilters) {
  return paginate<AdminReport>(token, '/api/v1/admin/reports', { ...filters })
}

export function getReport(token: string, id: string) {
  return json<AdminReport>(token, 'GET', `/api/v1/admin/reports/${id}`)
}

export function updateReport(
  token: string,
  id: string,
  params: { category?: ReportCategory; rule_ids?: string[] },
) {
  return json<AdminReport>(token, 'PATCH', `/api/v1/admin/reports/${id}`, params)
}

export type ReportTransition = 'assign_to_self' | 'unassign' | 'resolve' | 'reopen'

export function transitionReport(token: string, id: string, transition: ReportTransition) {
  return json<AdminReport>(token, 'POST', `/api/v1/admin/reports/${id}/${transition}`)
}

// ── Federation ─────────────────────────────────────────────────────────────

export type DomainBlockParams = Partial<Omit<DomainBlock, 'id' | 'digest' | 'created_at'>>

export function listDomainBlocks(token: string) {
  return paginate<DomainBlock>(token, '/api/v1/admin/domain_blocks')
}

export function createDomainBlock(token: string, params: DomainBlockParams) {
  return json<DomainBlock>(token, 'POST', '/api/v1/admin/domain_blocks', params)
}

export function updateDomainBlock(token: string, id: string, params: DomainBlockParams) {
  return json<DomainBlock>(token, 'PATCH', `/api/v1/admin/domain_blocks/${id}`, params)
}

export function deleteDomainBlock(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/domain_blocks/${id}`)
}

/**
 * The block a 422 from `createDomainBlock` says is in the way, if that is what
 * it says: `REST::Admin::ExistingDomainBlockErrorSerializer`.
 */
export function existingDomainBlock(e: unknown): DomainBlock | null {
  if (!(e instanceof AdminApiError) || e.status !== 422) return null
  const body = e.body as { existing_domain_block?: DomainBlock } | null
  return body?.existing_domain_block ?? null
}

export function listDomainAllows(token: string) {
  return paginate<DomainAllow>(token, '/api/v1/admin/domain_allows')
}

export function createDomainAllow(token: string, domain: string) {
  return json<DomainAllow>(token, 'POST', '/api/v1/admin/domain_allows', { domain })
}

export function deleteDomainAllow(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/domain_allows/${id}`)
}

// ── Blocks ─────────────────────────────────────────────────────────────────

export interface IpBlockParams {
  ip?: string
  severity?: IpBlockSeverity
  comment?: string
  /** Seconds from now; omitted for a block that never expires. */
  expires_in?: number
}

export function listIpBlocks(token: string) {
  return paginate<IpBlock>(token, '/api/v1/admin/ip_blocks')
}

export function createIpBlock(token: string, params: IpBlockParams) {
  return json<IpBlock>(token, 'POST', '/api/v1/admin/ip_blocks', params)
}

export function updateIpBlock(token: string, id: string, params: IpBlockParams) {
  return json<IpBlock>(token, 'PATCH', `/api/v1/admin/ip_blocks/${id}`, params)
}

export function deleteIpBlock(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/ip_blocks/${id}`)
}

export function listEmailDomainBlocks(token: string) {
  return paginate<EmailDomainBlock>(token, '/api/v1/admin/email_domain_blocks')
}

export function createEmailDomainBlock(
  token: string,
  params: { domain: string; allow_with_approval?: boolean },
) {
  return json<EmailDomainBlock>(token, 'POST', '/api/v1/admin/email_domain_blocks', params)
}

export function deleteEmailDomainBlock(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/email_domain_blocks/${id}`)
}

export function listCanonicalEmailBlocks(token: string) {
  return paginate<CanonicalEmailBlock>(token, '/api/v1/admin/canonical_email_blocks')
}

export function testCanonicalEmailBlock(token: string, email: string) {
  return json<CanonicalEmailBlock[]>(
    token,
    'POST',
    '/api/v1/admin/canonical_email_blocks/test',
    { email },
  )
}

export function createCanonicalEmailBlock(
  token: string,
  params: { email: string } | { canonical_email_hash: string },
) {
  return json<CanonicalEmailBlock>(
    token,
    'POST',
    '/api/v1/admin/canonical_email_blocks',
    params,
  )
}

export function deleteCanonicalEmailBlock(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/canonical_email_blocks/${id}`)
}

// ── Trends and hashtags ────────────────────────────────────────────────────

export function listTrendingTags(token: string) {
  return paginate<AdminTag>(token, '/api/v1/admin/trends/tags')
}

export function listTrendingStatuses(token: string) {
  return paginate<TrendStatus>(token, '/api/v1/admin/trends/statuses')
}

export function listTrendingLinks(token: string) {
  return paginate<TrendLink>(token, '/api/v1/admin/trends/links')
}

export function listPublishers(token: string) {
  return paginate<Publisher>(token, '/api/v1/admin/trends/links/publishers')
}

export type TrendKind = 'tags' | 'statuses' | 'links' | 'links/publishers'

export function reviewTrend<T>(
  token: string,
  kind: TrendKind,
  id: string,
  decision: 'approve' | 'reject',
) {
  return json<T>(token, 'POST', `/api/v1/admin/trends/${kind}/${id}/${decision}`)
}

export function listTags(token: string) {
  return paginate<AdminTag>(token, '/api/v1/admin/tags')
}

export function updateTag(
  token: string,
  id: string,
  params: Partial<Pick<AdminTag, 'trendable' | 'usable' | 'listable'>> & {
    display_name?: string
  },
) {
  return json<AdminTag>(token, 'PATCH', `/api/v1/admin/tags/${id}`, params)
}

// ── Custom emoji (eunha) ───────────────────────────────────────────────────

export function listCustomEmojis(token: string) {
  return json<AdminCustomEmoji[]>(token, 'GET', '/api/v1/admin/custom_emojis')
}

export function createCustomEmoji(
  token: string,
  params: { shortcode: string; image: File; category?: string },
) {
  const form = new FormData()
  form.append('shortcode', params.shortcode)
  form.append('image', params.image)
  if (params.category) form.append('category', params.category)
  return json<AdminCustomEmoji>(token, 'POST', '/api/v1/admin/custom_emojis', form)
}

export function updateCustomEmoji(
  token: string,
  id: string,
  params: Partial<Pick<AdminCustomEmoji, 'visible_in_picker' | 'disabled' | 'category'>>,
) {
  return json<AdminCustomEmoji>(token, 'PATCH', `/api/v1/admin/custom_emojis/${id}`, params)
}

export function deleteCustomEmoji(token: string, id: string) {
  return empty(token, 'DELETE', `/api/v1/admin/custom_emojis/${id}`)
}

// ── Dashboard ──────────────────────────────────────────────────────────────

export function getMeasures(
  token: string,
  keys: string[],
  startAt: string,
  endAt: string,
) {
  return json<Measure[]>(token, 'POST', '/api/v1/admin/measures', {
    keys,
    start_at: startAt,
    end_at: endAt,
  })
}

export function getDimensions(
  token: string,
  keys: string[],
  startAt: string,
  endAt: string,
  limit = 8,
) {
  return json<Dimension[]>(token, 'POST', '/api/v1/admin/dimensions', {
    keys,
    start_at: startAt,
    end_at: endAt,
    limit,
  })
}

export function getRetention(
  token: string,
  startAt: string,
  endAt: string,
  frequency: 'day' | 'month' = 'month',
) {
  return json<Cohort[]>(token, 'POST', '/api/v1/admin/retention', {
    start_at: startAt,
    end_at: endAt,
    frequency,
  })
}

// ── Email subscriptions ────────────────────────────────────────────────────
// What Mastodon's admin pages for email newsletters do, which eunha serves as
// REST (Mastodon has only the web forms).

/** A role that may offer email subscriptions. */
export interface EmailSubscriptionRole {
  id: string
  name: string
  color: string
  accounts: number
}

/**
 * `active`, `disabled` (turned off by the user), `no_access` (the role no
 * longer allows it), or `inactive`.
 */
export type EmailSubscriptionStatus = 'active' | 'disabled' | 'no_access' | 'inactive'

export interface EmailSubscriptionAccount {
  account: Account
  status: EmailSubscriptionStatus
  subscribers: number
  last_status_at: string | null
}

export interface EmailSubscriptionsOverview {
  /** Whether whoever runs the server lets the feature be enabled at all. */
  available: boolean
  enabled: boolean
  email_footer_text: string
  roles: EmailSubscriptionRole[]
  accounts: EmailSubscriptionAccount[]
}

export interface EmailSubscriber {
  id: string
  email: string
  created_at: string
  confirmed_at: string | null
}

const EMAIL_SUBSCRIPTIONS = '/api/v1/admin/email_subscriptions'

export function getEmailSubscriptions(token: string) {
  return json<EmailSubscriptionsOverview>(token, 'GET', EMAIL_SUBSCRIPTIONS)
}

export function setupEmailSubscriptions(
  token: string,
  agreements: { agreement_email_volume: boolean; agreement_privacy_and_terms: boolean },
) {
  return json<EmailSubscriptionsOverview>(
    token,
    'POST',
    `${EMAIL_SUBSCRIPTIONS}/setup`,
    agreements,
  )
}

export function disableEmailSubscriptions(token: string) {
  return json<EmailSubscriptionsOverview>(token, 'POST', `${EMAIL_SUBSCRIPTIONS}/disable`)
}

export function purgeEmailSubscriptions(token: string) {
  return json<EmailSubscriptionsOverview>(token, 'POST', `${EMAIL_SUBSCRIPTIONS}/purge`)
}

export function updateEmailFooterText(token: string, email_footer_text: string) {
  return json<EmailSubscriptionsOverview>(
    token,
    'PUT',
    `${EMAIL_SUBSCRIPTIONS}/additional_footer_text`,
    { email_footer_text },
  )
}

export function getEmailSubscriptionAccount(token: string, id: string) {
  return json<EmailSubscriptionAccount>(token, 'GET', `${EMAIL_SUBSCRIPTIONS}/accounts/${id}`)
}

export function setEmailSubscriptionAccount(token: string, id: string, enabled: boolean) {
  return json<EmailSubscriptionAccount>(
    token,
    'POST',
    `${EMAIL_SUBSCRIPTIONS}/accounts/${id}/${enabled ? 'enable' : 'disable'}`,
  )
}

export function listEmailSubscribers(token: string, id: string) {
  return paginate<EmailSubscriber>(token, `${EMAIL_SUBSCRIPTIONS}/accounts/${id}/subscriptions`)
}

export function deleteEmailSubscriber(token: string, id: string) {
  return empty(token, 'DELETE', `${EMAIL_SUBSCRIPTIONS}/${id}`)
}
