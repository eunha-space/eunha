// The server administration Mastodon has only as server-rendered admin pages
// (`Admin::SettingsController`, rules, roles, announcements, instances,
// relays, invites, webhooks, follow recommendations, software updates and the
// dashboard), as eunha serves it under `/api/v1/admin/`. Kept apart from
// `admin-api.ts`, which is Mastodon's own admin API, over the same transport.
import { json, query, request, type Account, type Measure, type Permission } from './admin-api.ts'

// ── Server settings ─────────────────────────────────────────────────────────

/** `SiteUpload`, as the settings show an uploaded image. */
export interface SiteUpload {
  id: string
  var: string
  url: string | null
  content_type: string | null
  file_size: number | null
  blurhash: string | null
  meta: { width?: number; height?: number } | null
  updated_at: string
}

/**
 * `Form::AdminSettings`: every key, as saved or as defaulted. The keys the
 * instance configuration decides whatever is saved are in `overridden`.
 */
export interface AdminSettings {
  site_contact_username: string
  site_contact_email: string
  site_title: string
  site_short_description: string
  site_extended_description: string
  site_terms: string
  registrations_mode: 'open' | 'approved' | 'none'
  closed_registrations_message: string
  bootstrap_timeline_accounts: string
  theme: string
  activity_api_enabled: boolean
  peers_api_enabled: boolean
  preview_sensitive_media: boolean
  custom_css: string
  profile_directory: boolean
  thumbnail: SiteUpload | null
  thumbnail_description: string
  mascot: SiteUpload | null
  trends: boolean
  trendable_by_default: boolean
  show_domain_blocks: 'disabled' | 'users' | 'all'
  show_domain_blocks_rationale: 'disabled' | 'users' | 'all'
  allow_referrer_origin: boolean
  noindex: boolean
  require_invite_text: boolean
  media_cache_retention_period: number | null
  content_cache_retention_period: number | null
  backups_retention_period: number | null
  status_page_url: string
  captcha_enabled: boolean
  authorized_fetch: boolean
  app_icon: SiteUpload | null
  favicon: SiteUpload | null
  min_age: number | null
  local_live_feed_access: string
  remote_live_feed_access: string
  local_topic_feed_access: string
  remote_topic_feed_access: string
  landing_page: string
  wrapstodon: boolean
  email_footer_text: string
  overridden: string[]
}

export function getAdminSettings(token: string) {
  return json<AdminSettings>(token, 'GET', '/api/v1/admin/settings')
}

/**
 * Save the keys given, and only those, as one settings page posts its own.
 * Files go in a multipart body; everything else is sent as text the way the
 * form sends it, booleans as `1` and `0`.
 */
export function updateAdminSettings(
  token: string,
  values: Record<string, string | number | boolean | null | File>,
) {
  const body = new FormData()
  for (const [key, value] of Object.entries(values)) {
    if (value instanceof File) body.append(key, value)
    else if (typeof value === 'boolean') body.append(key, value ? '1' : '0')
    else body.append(key, value === null ? '' : String(value))
  }
  return json<AdminSettings>(token, 'PATCH', '/api/v1/admin/settings', body)
}

export function deleteSiteUpload(token: string, id: string) {
  return json<Record<string, never>>(token, 'DELETE', `/api/v1/admin/site_uploads/${id}`)
}

// ── Rules ───────────────────────────────────────────────────────────────────

export interface AdminRuleTranslation {
  id: string
  language: string
  text: string
  hint: string
}

export interface AdminRule {
  id: string
  text: string
  hint: string
  priority: number
  translations: AdminRuleTranslation[]
  created_at: string
  updated_at: string
}

/** A rule's form: `translations_attributes` as Rails' nested attributes. */
export interface RuleParams {
  text: string
  hint: string
  translations_attributes: {
    id?: string
    language: string
    text: string
    hint: string
    _destroy?: boolean
  }[]
}

export function listRules(token: string) {
  return json<AdminRule[]>(token, 'GET', '/api/v1/admin/rules')
}

export function createRule(token: string, params: RuleParams) {
  return json<AdminRule>(token, 'POST', '/api/v1/admin/rules', params)
}

export function updateRule(token: string, id: string, params: RuleParams) {
  return json<AdminRule>(token, 'PATCH', `/api/v1/admin/rules/${id}`, params)
}

export function deleteRule(token: string, id: string) {
  return json<Record<string, never>>(token, 'DELETE', `/api/v1/admin/rules/${id}`)
}

/** `move_up` or `move_down`, answered with the rules in their new order. */
export function moveRule(token: string, id: string, direction: 'move_up' | 'move_down') {
  return json<AdminRule[]>(token, 'POST', `/api/v1/admin/rules/${id}/${direction}`)
}

// ── Roles ───────────────────────────────────────────────────────────────────

/** The id of `UserRole.everyone`. */
export const EVERYONE_ROLE_ID = '-99'

export interface AdminRole {
  id: string
  name: string
  /** The computed permissions, as `REST::RoleSerializer` sends them. */
  permissions: string
  color: string
  highlighted: boolean
  collection_limit: number
  position: number
  require_2fa: boolean
  /** The role's own permissions, by name. */
  permissions_as_keys: Permission[]
  everyone: boolean
  users_count: number
  can_update: boolean
  can_destroy: boolean
  created_at: string
  updated_at: string
}

export interface RoleParams {
  name?: string
  color?: string
  highlighted?: boolean
  position?: number
  require_2fa?: boolean
  collection_limit?: number
  permissions_as_keys?: Permission[]
}

/** `UserRole.assignable`, lowest first; the everyone role is fetched apart. */
export function listRoles(token: string) {
  return json<AdminRole[]>(token, 'GET', '/api/v1/admin/roles')
}

export function getRole(token: string, id: string) {
  return json<AdminRole>(token, 'GET', `/api/v1/admin/roles/${id}`)
}

export function createRole(token: string, params: RoleParams) {
  return json<AdminRole>(token, 'POST', '/api/v1/admin/roles', params)
}

export function updateRole(token: string, id: string, params: RoleParams) {
  return json<AdminRole>(token, 'PATCH', `/api/v1/admin/roles/${id}`, params)
}

export function deleteRole(token: string, id: string) {
  return json<Record<string, never>>(token, 'DELETE', `/api/v1/admin/roles/${id}`)
}

// ── Announcements ───────────────────────────────────────────────────────────

export interface AdminAnnouncement {
  id: string
  text: string
  content: string
  published: boolean
  published_at: string | null
  scheduled_at: string | null
  starts_at: string | null
  ends_at: string | null
  all_day: boolean
  notification_sent_at: string | null
  created_at: string
  updated_at: string
}

export interface AnnouncementParams {
  text: string
  scheduled_at: string
  starts_at: string
  ends_at: string
  all_day: boolean
}

export function listAnnouncements(token: string, filter?: 'published' | 'unpublished') {
  const q = filter ? `?${filter}=1` : ''
  return json<AdminAnnouncement[]>(token, 'GET', `/api/v1/admin/announcements${q}`)
}

export function createAnnouncement(token: string, params: AnnouncementParams) {
  return json<AdminAnnouncement>(token, 'POST', '/api/v1/admin/announcements', params)
}

export function updateAnnouncement(token: string, id: string, params: AnnouncementParams) {
  return json<AdminAnnouncement>(token, 'PATCH', `/api/v1/admin/announcements/${id}`, params)
}

export function deleteAnnouncement(token: string, id: string) {
  return json<Record<string, never>>(token, 'DELETE', `/api/v1/admin/announcements/${id}`)
}

export function setAnnouncementPublished(token: string, id: string, published: boolean) {
  return json<AdminAnnouncement>(
    token,
    'POST',
    `/api/v1/admin/announcements/${id}/${published ? 'publish' : 'unpublish'}`,
  )
}

export function previewAnnouncement(token: string, id: string) {
  return json<{ announcement: AdminAnnouncement; user_count: number }>(
    token,
    'GET',
    `/api/v1/admin/announcements/${id}/preview`,
  )
}

export function testAnnouncement(token: string, id: string) {
  return json<Record<string, never>>(token, 'POST', `/api/v1/admin/announcements/${id}/test`)
}

// ── Instances ───────────────────────────────────────────────────────────────

export interface AdminInstance {
  domain: string
  accounts_count: number
  domain_block: {
    id: string
    severity: 'silence' | 'suspend' | 'noop'
    reject_media: boolean
    reject_reports: boolean
    private_comment: string | null
    public_comment: string | null
    obfuscate: boolean
  } | null
  domain_allow: { id: string; created_at: string } | null
  unavailable: boolean
  unavailable_since: string | null
  failure_days: number | null
}

export interface InstanceNote {
  id: string
  content: string
  account: Account | null
  created_at: string
}

export interface AdminInstanceDetail extends AdminInstance {
  persisted: boolean
  purgeable: boolean
  availability: { date: string; failing: boolean }[]
  exhausted_deliveries_days: string[]
  moderation_notes: InstanceNote[]
}

export interface InstanceFilters {
  limited?: boolean
  by_domain?: string
  availability?: 'failing' | 'unavailable'
  page?: number
}

export function listInstances(token: string, filters: InstanceFilters) {
  return json<AdminInstance[]>(
    token,
    'GET',
    `/api/v1/admin/instances${query({ ...filters, limited: filters.limited ? '1' : undefined })}`,
  )
}

export function getInstance(token: string, domain: string) {
  return json<AdminInstanceDetail>(
    token,
    'GET',
    `/api/v1/admin/instances/${encodeURIComponent(domain)}`,
  )
}

export function instanceDeliveryAction(
  token: string,
  domain: string,
  action: 'clear_delivery_errors' | 'restart_delivery' | 'stop_delivery',
) {
  return json<AdminInstanceDetail>(
    token,
    'POST',
    `/api/v1/admin/instances/${encodeURIComponent(domain)}/${action}`,
  )
}

export function purgeInstance(token: string, domain: string) {
  return json<Record<string, never>>(
    token,
    'DELETE',
    `/api/v1/admin/instances/${encodeURIComponent(domain)}`,
  )
}

export function createInstanceNote(token: string, domain: string, content: string) {
  return json<AdminInstanceDetail>(
    token,
    'POST',
    `/api/v1/admin/instances/${encodeURIComponent(domain)}/moderation_notes`,
    { content },
  )
}

export function deleteInstanceNote(token: string, domain: string, id: string) {
  return json<Record<string, never>>(
    token,
    'DELETE',
    `/api/v1/admin/instances/${encodeURIComponent(domain)}/moderation_notes/${id}`,
  )
}

/** The instance measures of Mastodon's admin API, for one domain. */
export function getInstanceMeasures(
  token: string,
  domain: string,
  keys: string[],
  startAt: string,
  endAt: string,
) {
  const params: Record<string, unknown> = { keys, start_at: startAt, end_at: endAt }
  for (const key of keys) params[key] = { domain }
  return json<Measure[]>(token, 'POST', '/api/v1/admin/measures', params)
}

/** Download a CSV export as a file. */
export async function downloadExport(token: string, kind: 'domain_blocks' | 'domain_allows') {
  const res = await request(token, 'GET', `/api/v1/admin/export_${kind}/export`)
  const blob = await res.blob()
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `${kind}.csv`
  a.click()
  URL.revokeObjectURL(url)
}

export interface ImportedDomainBlock {
  domain: string
  severity: 'silence' | 'suspend' | 'noop'
  reject_media: boolean
  reject_reports: boolean
  private_comment: string
  public_comment: string | null
  obfuscate: boolean
}

export function importDomainBlocks(token: string, file: File) {
  const body = new FormData()
  body.append('data', file)
  return json<{ domain_blocks: ImportedDomainBlock[]; warning_domains: string[]; errors: string[] }>(
    token,
    'POST',
    '/api/v1/admin/export_domain_blocks/import',
    body,
  )
}

export function importDomainAllows(token: string, file: File) {
  const body = new FormData()
  body.append('data', file)
  return json<string[]>(token, 'POST', '/api/v1/admin/export_domain_allows/import', body)
}

// ── Relays ──────────────────────────────────────────────────────────────────

export interface AdminRelay {
  id: string
  inbox_url: string
  state: 'idle' | 'pending' | 'accepted' | 'rejected'
  enabled: boolean
  follow_activity_id: string | null
  created_at: string
  updated_at: string
}

export function listRelays(token: string) {
  return json<AdminRelay[]>(token, 'GET', '/api/v1/admin/relays')
}

export function createRelay(token: string, inboxUrl: string) {
  return json<AdminRelay>(token, 'POST', '/api/v1/admin/relays', { inbox_url: inboxUrl })
}

export function setRelayEnabled(token: string, id: string, enabled: boolean) {
  return json<AdminRelay>(
    token,
    'POST',
    `/api/v1/admin/relays/${id}/${enabled ? 'enable' : 'disable'}`,
  )
}

export function deleteRelay(token: string, id: string) {
  return json<Record<string, never>>(token, 'DELETE', `/api/v1/admin/relays/${id}`)
}

// ── Webhooks ────────────────────────────────────────────────────────────────

/** `Webhook::EVENTS`, with the permission each needs. */
export const WEBHOOK_EVENTS: { event: string; permission: Permission }[] = [
  { event: 'account.approved', permission: 'manage_users' },
  { event: 'account.created', permission: 'manage_users' },
  { event: 'account.updated', permission: 'manage_users' },
  { event: 'report.created', permission: 'manage_reports' },
  { event: 'report.updated', permission: 'manage_reports' },
  { event: 'status.created', permission: 'view_devops' },
  { event: 'status.updated', permission: 'view_devops' },
]

export interface AdminWebhook {
  id: string
  url: string
  events: string[]
  template: string | null
  enabled: boolean
  secret: string
  can_update: boolean
  created_at: string
  updated_at: string
}

export interface WebhookParams {
  url: string
  events: string[]
  template: string
}

export function listWebhooks(token: string) {
  return json<AdminWebhook[]>(token, 'GET', '/api/v1/admin/webhooks')
}

export function createWebhook(token: string, params: WebhookParams) {
  return json<AdminWebhook>(token, 'POST', '/api/v1/admin/webhooks', params)
}

export function updateWebhook(token: string, id: string, params: WebhookParams) {
  return json<AdminWebhook>(token, 'PATCH', `/api/v1/admin/webhooks/${id}`, params)
}

export function deleteWebhook(token: string, id: string) {
  return json<Record<string, never>>(token, 'DELETE', `/api/v1/admin/webhooks/${id}`)
}

export function webhookAction(
  token: string,
  id: string,
  action: 'enable' | 'disable' | 'secret/rotate',
) {
  return json<AdminWebhook>(token, 'POST', `/api/v1/admin/webhooks/${id}/${action}`)
}

// ── Follow recommendations ──────────────────────────────────────────────────

export interface AdminFollowRecommendation {
  account: Account
  reason: string[]
  rank: number | null
  language: string | null
  suppressed: boolean
}

export function listFollowRecommendations(
  token: string,
  params: { language?: string; status?: 'suppressed'; page?: number },
) {
  return json<AdminFollowRecommendation[]>(
    token,
    'GET',
    `/api/v1/admin/follow_recommendations${query(params)}`,
  )
}

export function setFollowRecommendationsSuppressed(
  token: string,
  accountIds: string[],
  suppressed: boolean,
) {
  return json<Record<string, never>>(
    token,
    'POST',
    `/api/v1/admin/follow_recommendations/${suppressed ? 'suppress' : 'unsuppress'}`,
    { account_ids: accountIds },
  )
}

// ── Software updates ────────────────────────────────────────────────────────

export interface SoftwareUpdate {
  version: string
  type: 'patch' | 'minor' | 'major'
  urgent: boolean
  release_notes: string
  end_of_support: string | null
}

/** A 404 when no update server is configured, as upstream's page is. */
export function listSoftwareUpdates(token: string) {
  return json<{ current_version: string; updates: SoftwareUpdate[] }>(
    token,
    'GET',
    '/api/v1/admin/software_updates',
  )
}

// ── Invites ─────────────────────────────────────────────────────────────────

export interface AdminInvite {
  id: string
  code: string
  url: string
  uses: number
  max_uses: number | null
  expires_at: string | null
  expired: boolean
  valid_for_use: boolean
  autofollow: boolean
  comment: string | null
  created_at: string
  account: Account | null
}

export function listAdminInvites(
  token: string,
  filter?: 'available' | 'expired',
  page = 1,
) {
  return json<AdminInvite[]>(
    token,
    'GET',
    `/api/v1/admin/invites${query({ [filter ?? '']: filter ? '1' : undefined, page })}`,
  )
}

export function deactivateAllInvites(token: string) {
  return json<Record<string, never>>(token, 'POST', '/api/v1/admin/invites/deactivate_all')
}

/** `DELETE /api/v1/invites/:id`, which a role with `manage_invites` may call on any. */
export async function expireInvite(token: string, id: string) {
  await request(token, 'DELETE', `/api/v1/invites/${id}`)
}

export function distributeAnnouncement(token: string, id: string) {
  return json<AdminAnnouncement>(
    token,
    'POST',
    `/api/v1/admin/announcements/${id}/distribution`,
  )
}
