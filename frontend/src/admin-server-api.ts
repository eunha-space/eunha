// The server administration Mastodon has only as server-rendered admin pages
// (`Admin::SettingsController`, rules, roles, announcements, instances,
// relays, invites, webhooks, follow recommendations, software updates and the
// dashboard), as eunha serves it under `/api/v1/admin/`. Kept apart from
// `admin-api.ts`, which is Mastodon's own admin API, over the same transport.
import { json } from './admin-api.ts'

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
