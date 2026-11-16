import { useEffect, useState, type FormEvent } from 'react'
import type { MessageDescriptor } from 'react-intl'
import { toast } from 'sonner'

import { type Permission } from '../../admin-api.ts'
import {
  createRole,
  deleteRole,
  EVERYONE_ROLE_ID,
  getRole,
  listRoles,
  updateRole,
  type AdminRole,
  type RoleParams,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/** `UserRole::Flags::CATEGORIES`, each privilege with its label and hint,
 *  as Mastodon's `admin.roles` messages. */
const CATEGORIES: {
  title: MessageDescriptor
  privileges: [Permission, MessageDescriptor, MessageDescriptor][]
}[] = [
  {
    title: { id: 'mastodon.admin.roles.categories.invites', defaultMessage: 'Invites' },
    privileges: [
      [
        'invite_users',
        { id: 'mastodon.admin.roles.privileges.invite_users', defaultMessage: 'Invite Users' },
        { id: 'mastodon.admin.roles.privileges.invite_users_description', defaultMessage: 'Allows users to invite new people to the server' },
      ],
      [
        'invite_bypass_approval',
        { id: 'mastodon.admin.roles.privileges.invite_bypass_approval', defaultMessage: 'Invite Users without review' },
        { id: 'mastodon.admin.roles.privileges.invite_bypass_approval_description', defaultMessage: 'Allows people invited to the server by these users to bypass moderation approval' },
      ],
    ],
  },
  {
    title: { id: 'mastodon.admin.roles.categories.email', defaultMessage: 'Email' },
    privileges: [
      [
        'manage_email_subscriptions',
        { id: 'mastodon.admin.roles.privileges.manage_email_subscriptions', defaultMessage: 'Manage Email Subscriptions' },
        { id: 'mastodon.admin.roles.privileges.manage_email_subscriptions_description', defaultMessage: 'Allow users with this permission to enable the email newsletter feature for their account' },
      ],
    ],
  },
  {
    title: { id: 'mastodon.admin.roles.categories.moderation', defaultMessage: 'Moderation' },
    privileges: [
      [
        'view_dashboard',
        { id: 'mastodon.admin.roles.privileges.view_dashboard', defaultMessage: 'View Dashboard' },
        { id: 'mastodon.admin.roles.privileges.view_dashboard_description', defaultMessage: 'Allows users to access the dashboard and various metrics' },
      ],
      [
        'view_audit_log',
        { id: 'mastodon.admin.roles.privileges.view_audit_log', defaultMessage: 'View Audit Log' },
        { id: 'mastodon.admin.roles.privileges.view_audit_log_description', defaultMessage: 'Allows users to see a history of administrative actions on the server' },
      ],
      [
        'manage_users',
        { id: 'mastodon.admin.roles.privileges.manage_users', defaultMessage: 'Manage Users' },
        { id: 'mastodon.admin.roles.privileges.manage_users_description', defaultMessage: 'Allows users to view other users\' details and perform moderation actions against them' },
      ],
      [
        'manage_user_access',
        { id: 'mastodon.admin.roles.privileges.manage_user_access', defaultMessage: 'Manage User Access' },
        { id: 'mastodon.admin.roles.privileges.manage_user_access_description', defaultMessage: 'Allows users to disable other users\' two-factor authentication, change their email address, and reset their password' },
      ],
      [
        'delete_user_data',
        { id: 'mastodon.admin.roles.privileges.delete_user_data', defaultMessage: 'Delete User Data' },
        { id: 'mastodon.admin.roles.privileges.delete_user_data_description', defaultMessage: 'Allows users to delete other users\' data without delay' },
      ],
      [
        'manage_reports',
        { id: 'mastodon.admin.roles.privileges.manage_reports', defaultMessage: 'Manage Reports' },
        { id: 'mastodon.admin.roles.privileges.manage_reports_description', defaultMessage: 'Allows users to review reports and perform moderation actions against them' },
      ],
      [
        'manage_appeals',
        { id: 'mastodon.admin.roles.privileges.manage_appeals', defaultMessage: 'Manage Appeals' },
        { id: 'mastodon.admin.roles.privileges.manage_appeals_description', defaultMessage: 'Allows users to review appeals against moderation actions' },
      ],
      [
        'manage_federation',
        { id: 'mastodon.admin.roles.privileges.manage_federation', defaultMessage: 'Manage Federation' },
        { id: 'mastodon.admin.roles.privileges.manage_federation_description', defaultMessage: 'Allows users to block or allow federation with other domains, and control deliverability' },
      ],
      [
        'manage_blocks',
        { id: 'mastodon.admin.roles.privileges.manage_blocks', defaultMessage: 'Manage Blocks' },
        { id: 'mastodon.admin.roles.privileges.manage_blocks_description', defaultMessage: 'Allows users to block email providers and IP addresses' },
      ],
      [
        'manage_taxonomies',
        { id: 'mastodon.admin.roles.privileges.manage_taxonomies', defaultMessage: 'Manage Taxonomies' },
        { id: 'mastodon.admin.roles.privileges.manage_taxonomies_description', defaultMessage: 'Allows users to review trending content and update hashtag settings' },
      ],
      [
        'manage_invites',
        { id: 'mastodon.admin.roles.privileges.manage_invites', defaultMessage: 'Manage Invites' },
        { id: 'mastodon.admin.roles.privileges.manage_invites_description', defaultMessage: 'Allows users to browse and deactivate invite links' },
      ],
      [
        'view_feeds',
        { id: 'mastodon.admin.roles.privileges.view_feeds', defaultMessage: 'View live and topic feeds' },
        { id: 'mastodon.admin.roles.privileges.view_feeds_description', defaultMessage: 'Allows users to access the live and topic feeds regardless of server settings' },
      ],
    ],
  },
  {
    title: { id: 'mastodon.admin.roles.categories.administration', defaultMessage: 'Administration' },
    privileges: [
      [
        'manage_settings',
        { id: 'mastodon.admin.roles.privileges.manage_settings', defaultMessage: 'Manage Settings' },
        { id: 'mastodon.admin.roles.privileges.manage_settings_description', defaultMessage: 'Allows users to change site settings' },
      ],
      [
        'manage_rules',
        { id: 'mastodon.admin.roles.privileges.manage_rules', defaultMessage: 'Manage Rules' },
        { id: 'mastodon.admin.roles.privileges.manage_rules_description', defaultMessage: 'Allows users to change server rules' },
      ],
      [
        'manage_roles',
        { id: 'mastodon.admin.roles.privileges.manage_roles', defaultMessage: 'Manage Roles' },
        { id: 'mastodon.admin.roles.privileges.manage_roles_description', defaultMessage: 'Allows users to manage and assign roles below theirs' },
      ],
      [
        'manage_webhooks',
        { id: 'mastodon.admin.roles.privileges.manage_webhooks', defaultMessage: 'Manage Webhooks' },
        { id: 'mastodon.admin.roles.privileges.manage_webhooks_description', defaultMessage: 'Allows users to set up webhooks for administrative events' },
      ],
      [
        'manage_custom_emojis',
        { id: 'mastodon.admin.roles.privileges.manage_custom_emojis', defaultMessage: 'Manage Custom Emojis' },
        { id: 'mastodon.admin.roles.privileges.manage_custom_emojis_description', defaultMessage: 'Allows users to manage custom emojis on the server' },
      ],
      [
        'manage_announcements',
        { id: 'mastodon.admin.roles.privileges.manage_announcements', defaultMessage: 'Manage Announcements' },
        { id: 'mastodon.admin.roles.privileges.manage_announcements_description', defaultMessage: 'Allows users to manage announcements on the server' },
      ],
    ],
  },
  {
    title: { id: 'mastodon.admin.roles.categories.devops', defaultMessage: 'DevOps' },
    privileges: [
      [
        'view_devops',
        { id: 'mastodon.admin.roles.privileges.view_devops', defaultMessage: 'DevOps' },
        {
          id: 'mastodon.admin.roles.privileges.view_devops_description',
          defaultMessage: 'Allows users to access Sidekiq and pgHero dashboards',
        },
      ],
    ],
  },
  {
    title: { id: 'mastodon.admin.roles.categories.special', defaultMessage: 'Special' },
    privileges: [
      [
        'administrator',
        { id: 'mastodon.admin.roles.privileges.administrator', defaultMessage: 'Administrator' },
        {
          id: 'mastodon.admin.roles.privileges.administrator_description',
          defaultMessage: 'Users with this permission will bypass every permission',
        },
      ],
    ],
  },
]

/** `Flags::SAFE`: all the everyone role may be given. */
const SAFE: Permission[] = ['invite_users', 'invite_bypass_approval']

function RoleForm({
  initial,
  onSaved,
  onCancel,
}: {
  initial: AdminRole | null
  onSaved: () => void
  onCancel: () => void
}) {
  const token = getToken()
  const everyone = initial?.everyone ?? false
  const [name, setName] = useState(initial?.name ?? '')
  const [color, setColor] = useState(initial?.color ?? '')
  const [highlighted, setHighlighted] = useState(initial?.highlighted ?? false)
  const [position, setPosition] = useState(String(initial?.position ?? 0))
  const [require2fa, setRequire2fa] = useState(initial?.require_2fa ?? false)
  const [collectionLimit, setCollectionLimit] = useState(String(initial?.collection_limit ?? 10))
  const [permissions, setPermissions] = useState<Permission[]>(initial?.permissions_as_keys ?? [])
  const [saving, setSaving] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    const params: RoleParams = everyone
      ? { permissions_as_keys: permissions }
      : {
          name,
          color,
          highlighted,
          position: Number(position),
          require_2fa: require2fa,
          collection_limit: Number(collectionLimit),
          permissions_as_keys: permissions,
        }
    try {
      if (initial) await updateRole(token ?? '', initial.id, params)
      else await createRole(token ?? '', params)
      toast.success(initial ? t({ id: 'eunha.admin.roles.role_saved', defaultMessage: 'Role saved.' }) : t({ id: 'eunha.admin.roles.role_created', defaultMessage: 'Role created.' }))
      onSaved()
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <form onSubmit={submit} className="space-y-4 rounded-lg border p-4">
      {!everyone && (
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="space-y-1">
            <Label htmlFor="role-name">{t({ id: 'mastodon.admin.tags.name', defaultMessage: 'Name' })}</Label>
            <Input id="role-name" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div className="space-y-1">
            <Label htmlFor="role-color">{t({ id: 'mastodon.simple_form.labels.user_role.color', defaultMessage: 'Badge color' })}</Label>
            <Input
              id="role-color"
              value={color}
              placeholder="#ff0000"
              onChange={(e) => setColor(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="role-position">{t({ id: 'mastodon.simple_form.labels.user_role.position', defaultMessage: 'Priority' })}</Label>
            <Input
              id="role-position"
              type="number"
              value={position}
              onChange={(e) => setPosition(e.target.value)}
            />
            <p className="text-muted-foreground text-xs">
              {t({ id: 'eunha.admin.roles.higher_role_decides_conflict_resolution_in', defaultMessage: 'Higher role decides conflict resolution in certain situations. Certain actions can only be performed on roles with a lower priority.' })}
            </p>
          </div>
          <div className="space-y-1">
            <Label htmlFor="role-collections">{t({ id: 'eunha.admin.roles.collection_limit', defaultMessage: 'Collection limit' })}</Label>
            <Input
              id="role-collections"
              type="number"
              min={0}
              value={collectionLimit}
              onChange={(e) => setCollectionLimit(e.target.value)}
            />
          </div>
          <Label className="text-sm font-normal">
            <Switch checked={highlighted} onCheckedChange={setHighlighted} />
            {t({ id: 'mastodon.simple_form.labels.user_role.highlighted', defaultMessage: 'Display role as badge on user profiles' })}
          </Label>
          <Label className="text-sm font-normal">
            <Switch checked={require2fa} onCheckedChange={setRequire2fa} />
            {t({ id: 'mastodon.simple_form.labels.user_role.require_2fa', defaultMessage: 'Require two-factor authentication' })}
          </Label>
        </div>
      )}
      {everyone && (
        <p className="text-muted-foreground text-sm">
          {t({ id: 'eunha.admin.roles.the_base_role_applies_to_every', defaultMessage: 'The base role applies to every user, including those without an assigned role. It may only carry the invite permissions.' })}
        </p>
      )}
      {CATEGORIES.map((category) => {
        const privileges = category.privileges.filter(
          ([key]) => !everyone || SAFE.includes(key),
        )
        if (privileges.length === 0) return null
        return (
          <fieldset key={category.title.id} className="space-y-2">
            <legend className="text-sm font-semibold">{t(category.title)}</legend>
            {privileges.map(([key, label, hint]) => (
              <Label key={key} className="items-start text-sm font-normal">
                <Checkbox
                  checked={permissions.includes(key)}
                  onCheckedChange={(on) =>
                    setPermissions((list) =>
                      on ? [...list, key] : list.filter((p) => p !== key),
                    )
                  }
                />
                <span>
                  <span className="font-medium">{t(label)}</span>
                  <span className="text-muted-foreground block text-xs">{t(hint)}</span>
                </span>
              </Label>
            ))}
          </fieldset>
        )
      })}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={saving}>
          {initial ? t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' }) : t({ id: 'mastodon.admin.roles.add_new', defaultMessage: 'Add role' })}
        </Button>
        <Button type="button" size="sm" variant="outline" onClick={onCancel}>
          {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
        </Button>
      </div>
    </form>
  )
}

/**
 * Roles: Mastodon's `Admin::RolesController`. A role may only be edited by a
 * role above it, and may grant only what its editor holds.
 */
export default function Roles() {
  const token = getToken()
  const [roles, setRoles] = useState<AdminRole[] | null>(null)
  const [everyone, setEveryone] = useState<AdminRole | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | 'new' | null>(null)

  const load = () => {
    if (!token) return
    listRoles(token)
      .then((r) => {
        // Mastodon lists the highest first.
        setRoles([...r].reverse())
        setError(null)
      })
      .catch((e) => setError(String(e)))
    getRole(token, EVERYONE_ROLE_ID)
      .then(setEveryone)
      .catch(() => setEveryone(null))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  const all = [...(roles ?? []), ...(everyone ? [everyone] : [])]

  return (
    <AdminLayout
      title={t({ id: 'mastodon.admin.roles.title', defaultMessage: 'Roles' })}
      permission="manage_roles"
      actions={
        editing === null && (
          <Button size="sm" onClick={() => setEditing('new')}>
            {t({ id: 'mastodon.admin.roles.add_new', defaultMessage: 'Add role' })}
          </Button>
        )
      }
    >
      <p className="text-muted-foreground mb-3 text-sm">
        {t({ id: 'eunha.admin.roles.with_user_roles_you_can_customize', defaultMessage: 'With user roles, you can customize which functions and areas your users can access.' })}
      </p>
      {editing === 'new' && (
        <div className="mb-4">
          <RoleForm
            initial={null}
            onCancel={() => setEditing(null)}
            onSaved={() => {
              setEditing(null)
              load()
            }}
          />
        </div>
      )}
      <AdminError error={error} />
      {roles === null && !error && <RowsSkeleton action />}
      <div className="space-y-2">
        {all.map((role) =>
          editing === role.id ? (
            <RoleForm
              key={role.id}
              initial={role}
              onCancel={() => setEditing(null)}
              onSaved={() => {
                setEditing(null)
                load()
              }}
            />
          ) : (
            <div key={role.id} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 text-sm font-medium">
                  {role.color && (
                    <span
                      className="inline-block size-3 rounded-full"
                      style={{ background: role.color }}
                      aria-hidden
                    />
                  )}
                  {role.everyone ? t({ id: 'eunha.admin.roles.base_role', defaultMessage: 'Base role' }) : role.name}
                  {role.highlighted && <Badge variant="outline">{t({ id: 'eunha.admin.roles.badge', defaultMessage: 'Badge' })}</Badge>}
                  {role.require_2fa && <Badge variant="outline">{t({ id: 'eunha.admin.roles.requires_2fa', defaultMessage: 'Requires 2FA' })}</Badge>}
                </div>
                <p className="text-muted-foreground text-xs">
                  {role.everyone
                    ? t({ id: 'eunha.admin.roles.all_users', defaultMessage: 'All users' })
                    : `${role.users_count} ${role.users_count === 1 ? 'user' : 'users'}`}
                  {' · '}
                  {role.permissions_as_keys.includes('administrator')
                    ? t({ id: 'mastodon.admin.roles.privileges.administrator', defaultMessage: 'Administrator' })
                    : `${role.permissions_as_keys.length} ${
                        role.permissions_as_keys.length === 1 ? 'permission' : 'permissions'
                      }`}
                </p>
              </div>
              {role.can_update && (
                <Button size="xs" variant="outline" onClick={() => setEditing(role.id)}>
                  {t({ id: 'status.edit', defaultMessage: 'Edit' })}
                </Button>
              )}
              {role.can_destroy && (
                <ConfirmButton
                  size="xs"
                  title={t({ id: 'eunha.admin.roles.delete', defaultMessage: 'Delete {name}?' }, { name: role.name })}
                  description={t({ id: 'eunha.admin.roles.its_users_are_left_with_the', defaultMessage: 'Its users are left with the base role.' })}
                  confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                  onConfirm={async () => {
                    await deleteRole(token ?? '', role.id)
                    toast.success(t({ id: 'eunha.admin.roles.role_deleted', defaultMessage: 'Role deleted.' }))
                    load()
                  }}
                >
                  {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                </ConfirmButton>
              )}
            </div>
          ),
        )}
      </div>
    </AdminLayout>
  )
}
