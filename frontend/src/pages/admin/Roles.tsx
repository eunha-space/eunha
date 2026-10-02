import { useEffect, useState, type FormEvent } from 'react'
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

/** `UserRole::Flags::CATEGORIES`, each privilege with its label and hint. */
const CATEGORIES: { title: string; privileges: [Permission, string, string][] }[] = [
  {
    title: 'Invites',
    privileges: [
      ['invite_users', 'Invite Users', 'Allows users to invite new people to the server'],
      [
        'invite_bypass_approval',
        'Invite Users without review',
        'Allows people invited to the server by these users to bypass moderation approval',
      ],
    ],
  },
  {
    title: 'Email',
    privileges: [
      [
        'manage_email_subscriptions',
        'Manage Email Subscriptions',
        'Allow users with this permission to enable the email newsletter feature for their account',
      ],
    ],
  },
  {
    title: 'Moderation',
    privileges: [
      ['view_dashboard', 'View Dashboard', 'Allows users to access the dashboard and various metrics'],
      ['view_audit_log', 'View Audit Log', 'Allows users to see a history of administrative actions on the server'],
      ['manage_users', 'Manage Users', "Allows users to view other users' details and perform moderation actions against them"],
      ['manage_user_access', 'Manage User Access', "Allows users to disable other users' two-factor authentication, change their email address, and reset their password"],
      ['delete_user_data', 'Delete User Data', "Allows users to delete other users' data without delay"],
      ['manage_reports', 'Manage Reports', 'Allows users to review reports and perform moderation actions against them'],
      ['manage_appeals', 'Manage Appeals', 'Allows users to review appeals against moderation actions'],
      ['manage_federation', 'Manage Federation', 'Allows users to block or allow federation with other domains, and control deliverability'],
      ['manage_blocks', 'Manage Blocks', 'Allows users to block email providers and IP addresses'],
      ['manage_taxonomies', 'Manage Taxonomies', 'Allows users to review trending content and update hashtag settings'],
      ['manage_invites', 'Manage Invites', 'Allows users to browse and deactivate invite links'],
      ['view_feeds', 'View live and topic feeds', 'Allows users to access the live and topic feeds regardless of server settings'],
    ],
  },
  {
    title: 'Administration',
    privileges: [
      ['manage_settings', 'Manage Settings', 'Allows users to change site settings'],
      ['manage_rules', 'Manage Rules', 'Allows users to change server rules'],
      ['manage_roles', 'Manage Roles', 'Allows users to manage and assign roles below theirs'],
      ['manage_webhooks', 'Manage Webhooks', 'Allows users to set up webhooks for administrative events'],
      ['manage_custom_emojis', 'Manage Custom Emojis', 'Allows users to manage custom emojis on the server'],
      ['manage_announcements', 'Manage Announcements', 'Allows users to manage announcements on the server'],
    ],
  },
  {
    title: 'DevOps',
    privileges: [['view_devops', 'DevOps', 'Allows users to access Sidekiq and pgHero dashboards']],
  },
  {
    title: 'Special',
    privileges: [['administrator', 'Administrator', 'Users with this permission will bypass every permission']],
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
      toast.success(initial ? 'Role saved.' : 'Role created.')
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
            <Label htmlFor="role-name">Name</Label>
            <Input id="role-name" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div className="space-y-1">
            <Label htmlFor="role-color">Badge color</Label>
            <Input
              id="role-color"
              value={color}
              placeholder="#ff0000"
              onChange={(e) => setColor(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="role-position">Priority</Label>
            <Input
              id="role-position"
              type="number"
              value={position}
              onChange={(e) => setPosition(e.target.value)}
            />
            <p className="text-muted-foreground text-xs">
              Higher role decides conflict resolution in certain situations. Certain actions
              can only be performed on roles with a lower priority.
            </p>
          </div>
          <div className="space-y-1">
            <Label htmlFor="role-collections">Collection limit</Label>
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
            Display role as badge on user profiles
          </Label>
          <Label className="text-sm font-normal">
            <Switch checked={require2fa} onCheckedChange={setRequire2fa} />
            Require two-factor authentication
          </Label>
        </div>
      )}
      {everyone && (
        <p className="text-muted-foreground text-sm">
          The base role applies to every user, including those without an assigned role. It may
          only carry the invite permissions.
        </p>
      )}
      {CATEGORIES.map((category) => {
        const privileges = category.privileges.filter(
          ([key]) => !everyone || SAFE.includes(key),
        )
        if (privileges.length === 0) return null
        return (
          <fieldset key={category.title} className="space-y-2">
            <legend className="text-sm font-semibold">{category.title}</legend>
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
                  <span className="font-medium">{label}</span>
                  <span className="text-muted-foreground block text-xs">{hint}</span>
                </span>
              </Label>
            ))}
          </fieldset>
        )
      })}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={saving}>
          {initial ? 'Save changes' : 'Add role'}
        </Button>
        <Button type="button" size="sm" variant="outline" onClick={onCancel}>
          Cancel
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
      title="Roles"
      permission="manage_roles"
      actions={
        editing === null && (
          <Button size="sm" onClick={() => setEditing('new')}>
            Add role
          </Button>
        )
      }
    >
      <p className="text-muted-foreground mb-3 text-sm">
        With user roles, you can customize which functions and areas your users can access.
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
      {roles === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
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
                  {role.everyone ? 'Base role' : role.name}
                  {role.highlighted && <Badge variant="outline">Badge</Badge>}
                  {role.require_2fa && <Badge variant="outline">Requires 2FA</Badge>}
                </div>
                <p className="text-muted-foreground text-xs">
                  {role.everyone
                    ? 'All users'
                    : `${role.users_count} ${role.users_count === 1 ? 'user' : 'users'}`}
                  {' · '}
                  {role.permissions_as_keys.includes('administrator')
                    ? 'Administrator'
                    : `${role.permissions_as_keys.length} ${
                        role.permissions_as_keys.length === 1 ? 'permission' : 'permissions'
                      }`}
                </p>
              </div>
              {role.can_update && (
                <Button size="xs" variant="outline" onClick={() => setEditing(role.id)}>
                  Edit
                </Button>
              )}
              {role.can_destroy && (
                <ConfirmButton
                  size="xs"
                  title={`Delete ${role.name}?`}
                  description="Its users are left with the base role."
                  confirmLabel="Delete"
                  onConfirm={async () => {
                    await deleteRole(token ?? '', role.id)
                    toast.success('Role deleted.')
                    load()
                  }}
                >
                  Delete
                </ConfirmButton>
              )}
            </div>
          ),
        )}
      </div>
    </AdminLayout>
  )
}
