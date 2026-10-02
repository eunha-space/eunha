import { can, type Permission } from '../admin-api.ts'

export interface AdminSection {
  to: string
  label: string
  permission: Permission
}

/**
 * The moderation sections, in Mastodon's order, each with the permission its
 * admin API checks: the policy each endpoint authorizes against, not a guess.
 */
export const ADMIN_SECTIONS: AdminSection[] = [
  { to: '/admin/dashboard', label: 'Dashboard', permission: 'view_dashboard' },
  { to: '/admin/reports', label: 'Reports', permission: 'manage_reports' },
  { to: '/admin/accounts', label: 'Accounts', permission: 'manage_users' },
  { to: '/admin/domain_blocks', label: 'Federation', permission: 'manage_federation' },
  { to: '/admin/ip_blocks', label: 'Blocks', permission: 'manage_blocks' },
  { to: '/admin/trends/links', label: 'Trends', permission: 'manage_taxonomies' },
  { to: '/admin/tags', label: 'Hashtags', permission: 'manage_taxonomies' },
  { to: '/admin/custom_emojis', label: 'Custom emoji', permission: 'manage_custom_emojis' },
  {
    to: '/admin/email_subscriptions',
    label: 'Email newsletters',
    permission: 'manage_settings',
  },
  { to: '/admin/terms_of_service', label: 'Terms of service', permission: 'manage_settings' },
  { to: '/admin/disputes/appeals', label: 'Appeals', permission: 'manage_appeals' },
  { to: '/admin/warning_presets', label: 'Warning presets', permission: 'manage_settings' },
  { to: '/admin/action_logs', label: 'Audit log', permission: 'view_audit_log' },
  // Server administration.
  { to: '/admin/settings/branding', label: 'Server settings', permission: 'manage_settings' },
]

/** The first section this role may open, for `/admin` and the rail's link. */
export function firstAdminSection(permissions: number): AdminSection | null {
  return ADMIN_SECTIONS.find((s) => can(permissions, s.permission)) ?? null
}
