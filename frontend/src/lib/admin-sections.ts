import { can, type Permission } from '../admin-api.ts'
import { t } from '@/i18n.tsx'

export interface AdminSection {
  to: string
  label: string
  permission: Permission
  /** The experimental feature the section needs on, as Mastodon's navigation hides FASP. */
  feature?: 'fasp' | 'moderation_subscriptions'
  /** Left out on an instance a hosting service runs, whose server is not the administrators' to tend. */
  server?: true
}

/**
 * The moderation sections, in Mastodon's order, each with the permission its
 * admin API checks: the policy each endpoint authorizes against, not a guess.
 */
export const ADMIN_SECTIONS: AdminSection[] = [
  { to: '/admin/dashboard', get label() { return t({ id: 'mastodon.admin.dashboard.title', defaultMessage: 'Dashboard' }) }, permission: 'view_dashboard' },
  { to: '/admin/reports', get label() { return t({ id: 'mastodon.admin.reports.title', defaultMessage: 'Reports' }) }, permission: 'manage_reports' },
  { to: '/admin/accounts', get label() { return t({ id: 'mastodon.admin.accounts.title', defaultMessage: 'Accounts' }) }, permission: 'manage_users' },
  { to: '/admin/domain_blocks', get label() { return t({ id: 'mastodon.admin.instances.title', defaultMessage: 'Federation' }) }, permission: 'manage_federation' },
  { to: '/admin/ip_blocks', get label() { return t({ id: 'mastodon.doorkeeper.grouped_scopes.title.blocks', defaultMessage: 'Blocks' }) }, permission: 'manage_blocks' },
  { to: '/admin/trends/links', get label() { return t({ id: 'mastodon.admin.settings.discovery.trends', defaultMessage: 'Trends' }) }, permission: 'manage_taxonomies' },
  { to: '/admin/tags', get label() { return t({ id: 'search_results.hashtags', defaultMessage: 'Hashtags' }) }, permission: 'manage_taxonomies' },
  { to: '/admin/custom_emojis', get label() { return t({ id: 'eunha.admin_sections.custom_emoji', defaultMessage: 'Custom emoji' }) }, permission: 'manage_custom_emojis' },
  {
    to: '/admin/email_subscriptions',
    get label() { return t({ id: 'mastodon.admin.email_subscriptions.index.title', defaultMessage: 'Email newsletters' }) },
    permission: 'manage_settings',
  },
  { to: '/admin/terms_of_service', get label() { return t({ id: 'footer.terms_of_service', defaultMessage: 'Terms of service' }) }, permission: 'manage_settings' },
  { to: '/admin/disputes/appeals', get label() { return t({ id: 'mastodon.admin.disputes.appeals.title', defaultMessage: 'Appeals' }) }, permission: 'manage_appeals' },
  { to: '/admin/warning_presets', get label() { return t({ id: 'eunha.admin_sections.warning_presets', defaultMessage: 'Warning presets' }) }, permission: 'manage_settings' },
  { to: '/admin/action_logs', get label() { return t({ id: 'mastodon.admin.action_logs.title', defaultMessage: 'Audit log' }) }, permission: 'view_audit_log' },
  // Server administration.
  { to: '/admin/settings/branding', get label() { return t({ id: 'mastodon.admin.settings.title', defaultMessage: 'Server settings' }) }, permission: 'manage_settings' },
  { to: '/admin/rules', get label() { return t({ id: 'about.rules', defaultMessage: 'Server rules' }) }, permission: 'manage_rules' },
  { to: '/admin/roles', get label() { return t({ id: 'mastodon.admin.roles.title', defaultMessage: 'Roles' }) }, permission: 'manage_roles' },
  { to: '/admin/announcements', get label() { return t({ id: 'mastodon.admin.announcements.title', defaultMessage: 'Announcements' }) }, permission: 'manage_announcements' },
  { to: '/admin/instances', get label() { return t({ id: 'eunha.admin_sections.instances', defaultMessage: 'Instances' }) }, permission: 'manage_federation' },
  { to: '/admin/relays', get label() { return t({ id: 'mastodon.admin.relays.title', defaultMessage: 'Relays' }) }, permission: 'manage_federation' },
  { to: '/admin/invites', get label() { return t({ id: 'mastodon.admin.invites.title', defaultMessage: 'Invites' }) }, permission: 'manage_invites' },
  { to: '/admin/webhooks', get label() { return t({ id: 'mastodon.admin.webhooks.title', defaultMessage: 'Webhooks' }) }, permission: 'manage_webhooks' },
  {
    to: '/admin/moderation_subscriptions',
    get label() { return t({ id: 'eunha.admin_sections.moderation_subscriptions', defaultMessage: 'Moderation subscriptions' }) },
    permission: 'manage_federation',
    feature: 'moderation_subscriptions',
  },
  { to: '/admin/fasp/providers', label: 'FASP', permission: 'manage_federation', feature: 'fasp' },
  {
    to: '/admin/follow_recommendations',
    get label() { return t({ id: 'mastodon.admin.follow_recommendations.title', defaultMessage: 'Follow recommendations' }) },
    permission: 'manage_taxonomies',
  },
  { to: '/admin/software_updates', get label() { return t({ id: 'eunha.admin_sections.software_updates', defaultMessage: 'Software updates' }) }, permission: 'view_devops', server: true },
  { to: '/admin/server_checks', get label() { return t({ id: 'eunha.admin_sections.server_health', defaultMessage: 'Server health' }) }, permission: 'view_dashboard', server: true },
]

/** The first section this role may open, for `/admin` and the rail's link. */
export function firstAdminSection(permissions: number): AdminSection | null {
  return ADMIN_SECTIONS.find((s) => !s.feature && can(permissions, s.permission)) ?? null
}
