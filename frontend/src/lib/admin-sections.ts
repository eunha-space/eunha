import { can, type Permission } from '../admin-api.ts'
import {
  Ban, Download, FileClock, Flag, Gavel, Globe, Hash, HeartPulse, LayoutDashboard, ListChecks, Mail,
  Megaphone, MessageSquareWarning, Plug, RadioTower, Rss, ScrollText, Server, Settings, Smile, Ticket,
  TrendingUp, UserCog, UserPlus, Users, Webhook, type LucideIcon,
} from 'lucide-react'
import { t } from '@/i18n.tsx'

/** One of a section's pages, listed under it at `/admin`. */
export interface AdminPage {
  to: string
  label: string
}

export interface AdminSection {
  to: string
  icon: LucideIcon
  label: string
  /** The section's pages, when it has more than one; `to` is the first. */
  pages?: AdminPage[]
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
  { to: '/admin/dashboard', icon: LayoutDashboard, get label() { return t({ id: 'mastodon.admin.dashboard.title', defaultMessage: 'Dashboard' }) }, permission: 'view_dashboard' },
  { to: '/admin/reports', icon: Flag, get label() { return t({ id: 'mastodon.admin.reports.title', defaultMessage: 'Reports' }) }, permission: 'manage_reports' },
  { to: '/admin/accounts', icon: Users, get label() { return t({ id: 'mastodon.admin.accounts.title', defaultMessage: 'Accounts' }) }, permission: 'manage_users' },
  { to: '/admin/domain_blocks', icon: Globe, get label() { return t({ id: 'mastodon.admin.instances.title', defaultMessage: 'Federation' }) }, permission: 'manage_federation', pages: [
    { to: '/admin/domain_blocks', get label() { return t({ id: 'mastodon.exports.domain_blocks', defaultMessage: 'Domain blocks' }) } },
    { to: '/admin/domain_allows', get label() { return t({ id: 'eunha.admin.domain_blocks.domain_allows', defaultMessage: 'Domain allows' }) } },
  ] },
  { to: '/admin/ip_blocks', icon: Ban, get label() { return t({ id: 'mastodon.doorkeeper.grouped_scopes.title.blocks', defaultMessage: 'Blocks' }) }, permission: 'manage_blocks', pages: [
    { to: '/admin/ip_blocks', get label() { return t({ id: 'mastodon.admin.ip_blocks.title', defaultMessage: 'IP rules' }) } },
    { to: '/admin/email_domain_blocks', get label() { return t({ id: 'eunha.admin.ip_blocks.email_domains', defaultMessage: 'Email domains' }) } },
    { to: '/admin/canonical_email_blocks', get label() { return t({ id: 'eunha.admin.ip_blocks.email_addresses', defaultMessage: 'Email addresses' }) } },
    { to: '/admin/username_blocks', get label() { return t({ id: 'eunha.admin.ip_blocks.usernames', defaultMessage: 'Usernames' }) } },
  ] },
  { to: '/admin/trends/links', icon: TrendingUp, get label() { return t({ id: 'mastodon.admin.settings.discovery.trends', defaultMessage: 'Trends' }) }, permission: 'manage_taxonomies', pages: [
    { to: '/admin/trends/links', get label() { return t({ id: 'eunha.admin.trends.links', defaultMessage: 'Links' }) } },
    { to: '/admin/trends/links/preview_card_providers', get label() { return t({ id: 'mastodon.admin.trends.preview_card_providers.title', defaultMessage: 'Publishers' }) } },
    { to: '/admin/trends/statuses', get label() { return t({ id: 'account.posts', defaultMessage: 'Posts' }) } },
    { to: '/admin/trends/tags', get label() { return t({ id: 'search_results.hashtags', defaultMessage: 'Hashtags' }) } },
  ] },
  { to: '/admin/tags', icon: Hash, get label() { return t({ id: 'search_results.hashtags', defaultMessage: 'Hashtags' }) }, permission: 'manage_taxonomies' },
  { to: '/admin/custom_emojis', icon: Smile, get label() { return t({ id: 'eunha.admin_sections.custom_emoji', defaultMessage: 'Custom emoji' }) }, permission: 'manage_custom_emojis' },
  {
    to: '/admin/email_subscriptions', icon: Mail,
    get label() { return t({ id: 'mastodon.admin.email_subscriptions.index.title', defaultMessage: 'Email newsletters' }) },
    permission: 'manage_settings',
  },
  { to: '/admin/terms_of_service', icon: ScrollText, get label() { return t({ id: 'footer.terms_of_service', defaultMessage: 'Terms of service' }) }, permission: 'manage_settings', pages: [
    { to: '/admin/terms_of_service', get label() { return t({ id: 'mastodon.admin.terms_of_service.current', defaultMessage: 'Current' }) } },
    { to: '/admin/terms_of_service/draft', get label() { return t({ id: 'mastodon.admin.terms_of_service.draft', defaultMessage: 'Draft' }) } },
    { to: '/admin/terms_of_service/history', get label() { return t({ id: 'mastodon.admin.terms_of_service.history', defaultMessage: 'History' }) } },
  ] },
  { to: '/admin/disputes/appeals', icon: Gavel, get label() { return t({ id: 'mastodon.admin.disputes.appeals.title', defaultMessage: 'Appeals' }) }, permission: 'manage_appeals' },
  { to: '/admin/warning_presets', icon: MessageSquareWarning, get label() { return t({ id: 'eunha.admin_sections.warning_presets', defaultMessage: 'Warning presets' }) }, permission: 'manage_settings' },
  { to: '/admin/action_logs', icon: FileClock, get label() { return t({ id: 'mastodon.admin.action_logs.title', defaultMessage: 'Audit log' }) }, permission: 'view_audit_log' },
  // Server administration, which `/admin` lists under its own heading.
  { to: '/admin/settings/branding', icon: Settings, get label() { return t({ id: 'mastodon.admin.settings.title', defaultMessage: 'Server settings' }) }, permission: 'manage_settings', pages: [
    { to: '/admin/settings/branding', get label() { return t({ id: 'mastodon.admin.settings.branding.title', defaultMessage: 'Branding' }) } },
    { to: '/admin/settings/about', get label() { return t({ id: 'column.about', defaultMessage: 'About' }) } },
    { to: '/admin/settings/registrations', get label() { return t({ id: 'mastodon.admin.settings.registrations.title', defaultMessage: 'Registrations' }) } },
    { to: '/admin/settings/discovery', get label() { return t({ id: 'mastodon.admin.settings.discovery.title', defaultMessage: 'Discovery' }) } },
    { to: '/admin/settings/content_retention', get label() { return t({ id: 'mastodon.admin.settings.content_retention.title', defaultMessage: 'Content retention' }) } },
    { to: '/admin/settings/appearance', get label() { return t({ id: 'mastodon.admin.settings.appearance.title', defaultMessage: 'Appearance' }) } },
  ] },
  { to: '/admin/rules', icon: ListChecks, get label() { return t({ id: 'about.rules', defaultMessage: 'Server rules' }) }, permission: 'manage_rules' },
  { to: '/admin/roles', icon: UserCog, get label() { return t({ id: 'mastodon.admin.roles.title', defaultMessage: 'Roles' }) }, permission: 'manage_roles' },
  { to: '/admin/announcements', icon: Megaphone, get label() { return t({ id: 'mastodon.admin.announcements.title', defaultMessage: 'Announcements' }) }, permission: 'manage_announcements' },
  { to: '/admin/instances', icon: Server, get label() { return t({ id: 'eunha.admin_sections.instances', defaultMessage: 'Instances' }) }, permission: 'manage_federation' },
  { to: '/admin/relays', icon: RadioTower, get label() { return t({ id: 'mastodon.admin.relays.title', defaultMessage: 'Relays' }) }, permission: 'manage_federation' },
  { to: '/admin/invites', icon: Ticket, get label() { return t({ id: 'mastodon.admin.invites.title', defaultMessage: 'Invites' }) }, permission: 'manage_invites' },
  { to: '/admin/webhooks', icon: Webhook, get label() { return t({ id: 'mastodon.admin.webhooks.title', defaultMessage: 'Webhooks' }) }, permission: 'manage_webhooks' },
  {
    to: '/admin/moderation_subscriptions', icon: Rss,
    get label() { return t({ id: 'eunha.admin_sections.moderation_subscriptions', defaultMessage: 'Moderation subscriptions' }) },
    permission: 'manage_federation', pages: [
    { to: '/admin/moderation_subscriptions', get label() { return t({ id: 'eunha.admin.moderation_subscriptions.subscriptions', defaultMessage: 'Subscriptions' }) } },
    { to: '/admin/moderation_suggestions', get label() { return t({ id: 'eunha.admin.moderation_subscriptions.suggestions', defaultMessage: 'Suggestions' }) } },
    { to: '/admin/moderation_subscriptions/logs', get label() { return t({ id: 'eunha.admin.moderation_subscriptions.logs', defaultMessage: 'Logs' }) } },
  ],
    feature: 'moderation_subscriptions',
  },
  { to: '/admin/fasp/providers', icon: Plug, label: 'FASP', permission: 'manage_federation', pages: [
    { to: '/admin/fasp/providers', get label() { return t({ id: 'mastodon.admin.fasp.providers.providers', defaultMessage: 'Providers' }) } },
    { to: '/admin/fasp/debug/callbacks', get label() { return t({ id: 'eunha.admin.fasp.debug_callbacks', defaultMessage: 'Debug callbacks' }) } },
  ], feature: 'fasp' },
  {
    to: '/admin/follow_recommendations', icon: UserPlus,
    get label() { return t({ id: 'mastodon.admin.follow_recommendations.title', defaultMessage: 'Follow recommendations' }) },
    permission: 'manage_taxonomies',
  },
  { to: '/admin/software_updates', icon: Download, get label() { return t({ id: 'eunha.admin_sections.software_updates', defaultMessage: 'Software updates' }) }, permission: 'view_devops', server: true },
  { to: '/admin/server_checks', icon: HeartPulse, get label() { return t({ id: 'eunha.admin_sections.server_health', defaultMessage: 'Server health' }) }, permission: 'view_dashboard', server: true },
]

/** The first section of the server administration group. */
export const SERVER_GROUP_START = '/admin/settings/branding'

/** The first section this role may open: whether the rail offers `/admin` at all. */
export function firstAdminSection(permissions: number): AdminSection | null {
  return ADMIN_SECTIONS.find((s) => !s.feature && can(permissions, s.permission)) ?? null
}
